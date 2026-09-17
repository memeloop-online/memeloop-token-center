import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import type { RequestEvent } from '../src/types.js';
import { coalesceRequestEvent, mergeBatchedRequestPage } from '../src/operator/traffic/requestRefresh.js';
import { requestViewFromEvent } from '../src/operator/traffic/requestTraffic.js';

const event = (id: string, at: number, kind: RequestEvent['event_kind'] = 'started'): RequestEvent => ({
  event_id: String(at).padStart(8, '0'), event_at: at, event_kind: kind, request_id: id,
  key_id: 'key', created_at: 1, protocol: 'openai', model: 'model', status_code: kind === 'finished' ? 200 : null,
  completed_at: kind === 'finished' ? at : null, duration_ms: kind === 'finished' ? 10 : null,
  input_tokens: 1, output_tokens: kind === 'finished' ? 2 : 0, cost: '0', error_code: null, archive_state: 'pending',
});

test('events buffered during the pause tier merge coalesced on resume', () => {
  const snapshot = [requestViewFromEvent(event('visible', 1))!];
  // Simulates the bounded hook buffer accumulating while the page is paused:
  // one coalesced entry per request, latest terminal state preserved.
  const buffered = new Map<string, RequestEvent>();
  for (const incoming of [event('visible', 2), event('visible', 3, 'finished'), event('fresh', 4, 'finished')]) {
    buffered.set(incoming.request_id, coalesceRequestEvent(buffered.get(incoming.request_id), incoming));
  }
  const resumed = mergeBatchedRequestPage(snapshot, buffered, false);
  const visible = resumed.requests.find((request) => request.request_id === 'visible');
  assert.equal(visible?.status_code, 200);
  assert.equal(visible?.output_tokens, 2);
  assert.ok(resumed.requests.some((request) => request.request_id === 'fresh'));
});

test('filtered history stays stable: SSE batches never auto-refresh or merge it', async () => {
  const page = await readFile(new URL('../src/operator/pages/RequestsPage.tsx', import.meta.url), 'utf8');
  // The stream-revision effect must not schedule any background query for a
  // filtered view; it may only raise the explicit stale hint.
  const mergeEffect = page.slice(
    page.indexOf('if (streamPaused || liveEvents.size === 0) return;'),
    page.indexOf('}, [streamRevision, streamPaused]);'),
  );
  assert.match(mergeEffect, /typedFiltersActive\(filters\)/);
  assert.match(mergeEffect, /setOlderFilteredResultsStale\(true\)/);
  assert.doesNotMatch(mergeEffect, /scheduleOverflowRefresh/);
  assert.doesNotMatch(mergeEffect, /mergeRefreshedRequestPage/);
  // Overflow reconciliation only serves the unfiltered live first page.
  const overflowEffect = page.slice(page.indexOf('if (streamOverflowRevision !== previousOverflowRevision.current)'));
  assert.match(overflowEffect, /if \(typedFiltersActive\(filters\) \|\| loadedHistoryIds\.current\.size\) return;/);
  // The overflow refresh path is unreachable for filtered scopes and has no
  // leftover filtered merge branch.
  assert.match(page, /typedFiltersActive\(currentScope\.filters\) \|\| !reconcileOverflow\.current\) return;/);
  assert.doesNotMatch(page, /mergeRefreshedRequestPage\(current, next, olderFilteredResultsVisible\.current\)/);
  // No dead identifiers from the removed filtered auto-refresh remain.
  assert.doesNotMatch(page, /scheduleFilteredRefresh|refreshFilteredRequests|cancelFilteredRefresh|filteredRefresh\./);
  // The explicit manual refresh path for filtered results is preserved.
  assert.match(page, /onRefreshFilteredResults/);
  assert.match(page, /olderFilteredResultsStale/);
});

test('pause tier freezes UI merges, keeps the stream alive, and tells one consistent story', async () => {
  const page = await readFile(new URL('../src/operator/pages/RequestsPage.tsx', import.meta.url), 'utf8');
  const operator = await readFile(new URL('../src/operator/hooks/useOperatorRequestStream.ts', import.meta.url), 'utf8');
  assert.match(page, /userPaused \|\| \(requestRefresh\?\.paused \?\? false\)/);
  assert.match(page, /if \(streamPaused \|\| liveEvents\.size === 0\) return;/);
  assert.match(page, /\}, \[streamPaused\]\);/);
  assert.match(page, /if \(streamPaused\) return;/);
  assert.match(page, /refreshPaused=\{userPaused\}/);
  assert.match(page, /暂停更新/);
  assert.match(page, /恢复更新/);
  // The cadence control receives the effective pause state, so its hint never
  // claims list and summary update together while the pause toggle is active.
  assert.match(page, /paused=\{requestRefresh\.paused \|\| refreshPaused\}/);
  assert.match(page, /pausedHint=\{refreshPaused && !requestRefresh\.paused/);
  assert.match(page, /已暂停更新：事件接收继续，恢复后合并。/);
  // The duplicate hint next to the toggle was removed in favour of the control hint.
  assert.doesNotMatch(page, /已暂停：事件持续接收并在有界缓冲中暂存/);
  // The stream itself is never disabled by the page pause tier: SSE keeps
  // flowing into the bounded batch and event map while the UI is frozen.
  assert.match(operator, /enabled: enabled && !paused/);
  assert.match(operator, /batch\.current\?\.enqueue\(event\)/);
  assert.match(operator, /2_000/);
});

test('pause control renders inline with the cadence slider, no extra card or column', async () => {
  const page = await readFile(new URL('../src/operator/pages/RequestsPage.tsx', import.meta.url), 'utf8');
  const css = await readFile(new URL('../src/operator/traffic/RequestRefreshControl.css', import.meta.url), 'utf8');
  assert.match(page, /request-refresh-row/);
  assert.match(page, /<ToggleButton appearance="secondary" checked=\{refreshPaused\}/);
  assert.match(css, /\.request-refresh-row/);
  assert.match(css, /\.request-refresh-pause/);
});

test('tenant/token switch clears stale cursors, buffers and protection before rebuild', async () => {
  const page = await readFile(new URL('../src/operator/pages/RequestsPage.tsx', import.meta.url), 'utf8');
  const operator = await readFile(new URL('../src/operator/hooks/useOperatorRequestStream.ts', import.meta.url), 'utf8');
  // The stream hook drops old-scope protected ids before the new batch and
  // event map are established, so stale ids cannot shield old rows or widen
  // the new scope's protected set.
  assert.match(operator, /protectedIds\.current = \[\];/);
  assert.ok(operator.indexOf('protectedIds.current = [];') < operator.indexOf('events.current.clear()'));
  assert.ok(operator.indexOf('protectedIds.current = [];') < operator.indexOf('new RequestRefreshBatch('));
  // The page scope effect resets every scope-bound ref up front and releases
  // the hook-side protection before reloading the new scope.
  assert.match(page, /loadedHistoryIds\.current\.clear\(\);/);
  assert.match(page, /reconcileOverflow\.current = false;/);
  assert.match(page, /previousOverflowRevision\.current = streamOverflowRevision;/);
  assert.match(page, /onProtectRequests\?\.\(\[\]\);/);
  assert.ok(page.indexOf('loadedHistoryIds.current.clear();') < page.indexOf('void load(emptyTypedFilterAst);'));
  // The overflow baseline sync happens inside the scope effect, before the
  // overflow watcher can compare against it.
  const scopeEffect = page.slice(page.indexOf('loadedHistoryIds.current.clear();'), page.indexOf('void load(emptyTypedFilterAst);'));
  assert.match(scopeEffect, /previousOverflowRevision\.current = streamOverflowRevision;/);
});
