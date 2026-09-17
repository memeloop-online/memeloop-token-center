import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import type { RequestEvent } from '../src/types.js';
import { coalesceRequestEvent, mergeBatchedRequestPage, RequestRefreshBatch, trimRequestEventCache } from '../src/operator/traffic/requestRefresh.js';
import { requestViewFromEvent } from '../src/operator/traffic/requestTraffic.js';

function clock() {
  let now = 0; let id = 0;
  const timers = new Map<number, { at: number; run: () => void }>();
  return {
    schedule(run: () => void, delay: number) { timers.set(++id, { at: now + delay, run }); return id; },
    cancel(timer: number) { timers.delete(timer); },
    advance(ms: number) { now += ms; for (const [key, timer] of [...timers]) if (timer.at <= now) { timers.delete(key); timer.run(); } },
    get size() { return timers.size; },
  };
}

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

test('a paused batch publishes nothing, then releases one coalesced batch at the selected cadence on resume', () => {
  const time = clock();
  const published: Map<string, RequestEvent>[] = [];
  const batch = new RequestRefreshBatch(0, time, (values) => published.push(values));
  batch.enqueue(event('request', 1));
  batch.setPaused(true);
  // Realtime cadence + high traffic: while paused, no timer fires and no
  // publish (hence no revision bump and no rerender) can happen.
  for (let i = 2; i <= 500; i++) batch.enqueue(event('request', i));
  batch.enqueue(event('other', 600, 'finished'));
  time.advance(60000);
  assert.equal(published.length, 0);
  assert.equal(time.size, 0);
  batch.setPaused(false);
  time.advance(15);
  assert.equal(published.length, 0);
  time.advance(1);
  assert.equal(published.length, 1);
  assert.equal(published[0].get('request')?.event_id, String(500).padStart(8, '0'));
  assert.equal(published[0].get('other')?.status_code, 200);
  batch.dispose();
});

test('buffering while paused stays strictly bounded and reports overflow on the resume publish', () => {
  const time = clock();
  const published: { events: Map<string, RequestEvent>; overflow: boolean }[] = [];
  const batch = new RequestRefreshBatch(5000, time, (events, overflow) => published.push({ events, overflow }), 2);
  batch.protect(['visible']);
  batch.setPaused(true);
  batch.enqueue(event('visible', 1, 'finished'));
  for (let i = 2; i < 100; i++) batch.enqueue(event(String(i), i));
  time.advance(300000);
  assert.equal(published.length, 0);
  batch.setPaused(false);
  time.advance(5000);
  assert.equal(published.length, 1);
  assert.equal(published[0].events.size, 3);
  assert.equal(published[0].events.get('visible')?.status_code, 200);
  assert.equal(published[0].overflow, true);
  batch.dispose();
});

test('trimRequestEventCache enforces the bound, preserves protected terminals and is idempotent', () => {
  const cache = new Map<string, RequestEvent>();
  cache.set('visible', event('visible', 1, 'finished'));
  for (let i = 2; i <= 5; i++) cache.set(String(i), event(String(i), i));
  assert.equal(trimRequestEventCache(cache, new Set(['visible']), 2), true);
  assert.equal(cache.size, 3);
  assert.equal(cache.get('visible')?.status_code, 200);
  assert.deepEqual([...cache.keys()], ['visible', '4', '5']);
  // A second pass drops nothing: callers must not loop overflow bumps.
  assert.equal(trimRequestEventCache(cache, new Set(['visible']), 2), false);
});

test('shrinking the protected set trims pending immediately and flags overflow for the next publish', () => {
  const time = clock();
  const published: { events: Map<string, RequestEvent>; overflow: boolean }[] = [];
  const batch = new RequestRefreshBatch(5000, time, (events, overflow) => published.push({ events, overflow }), 2);
  batch.protect(['a', 'b']);
  for (const [index, id] of ['a', 'b', 'c', 'd'].entries()) batch.enqueue(event(id, index + 1));
  batch.protect([]);
  time.advance(5000);
  assert.equal(published.length, 1);
  assert.equal(published[0].events.size, 2);
  assert.deepEqual([...published[0].events.keys()], ['c', 'd']);
  assert.equal(published[0].overflow, true);
  batch.dispose();
});

// React wiring below cannot be exercised without a renderer. Keep only a few
// short, semantic source contracts; behavior lives in the tests above.

test('page pause tier reaches the hook batch while SSE and session events keep flowing', async () => {
  const page = await readFile(new URL('../src/operator/pages/RequestsPage.tsx', import.meta.url), 'utf8');
  const operator = await readFile(new URL('../src/operator/hooks/useOperatorRequestStream.ts', import.meta.url), 'utf8');
  assert.match(page, /onProtectRequests\?\.\(.{0,200}userPaused\)/);
  assert.match(operator, /setPaused\(paused \|\| !enabled \|\| consumerPause\.current\)/);
  assert.match(operator, /enabled: enabled && !paused/);
  assert.match(operator, /batch\.current\?\.enqueue\(event\)/);
  assert.match(operator, /setCadence\(intervalMs, paused \|\| !enabled\)/);
});

test('the open detail stays protected even outside the rendered window', async () => {
  const page = await readFile(new URL('../src/operator/pages/RequestsPage.tsx', import.meta.url), 'utf8');
  assert.match(page, /selectedRequest && !visibleIds\.includes\(selectedRequest\)/);
  assert.match(page, /setSelectedRequest\(requestId\)/);
  assert.match(page, /setSelectedRequest\(undefined\)/);
});

test('tenant/token switch clears stale cursors, buffers and protection', async () => {
  const page = await readFile(new URL('../src/operator/pages/RequestsPage.tsx', import.meta.url), 'utf8');
  const operator = await readFile(new URL('../src/operator/hooks/useOperatorRequestStream.ts', import.meta.url), 'utf8');
  assert.match(operator, /protectedIds\.current = \[\]/);
  assert.match(page, /loadedHistoryIds\.current\.clear\(\)/);
  assert.match(page, /onProtectRequests\?\.\(\[\]\)/);
});

test('protecting fewer ids trims the published cache and reports the loss for reconciliation', async () => {
  const operator = await readFile(new URL('../src/operator/hooks/useOperatorRequestStream.ts', import.meta.url), 'utf8');
  assert.match(operator, /trimRequestEventCache\(events\.current,/);
  assert.match(operator, /setOverflowRevision\(value => value \+ 1\)/);
});

test('pause control copy comes from the i18n catalogs in both locales', async () => {
  const page = await readFile(new URL('../src/operator/pages/RequestsPage.tsx', import.meta.url), 'utf8');
  const i18n = await readFile(new URL('../src/i18n.tsx', import.meta.url), 'utf8');
  for (const key of ['traffic.pauseUpdates', 'traffic.resumeUpdates', 'traffic.updatesPausedHint', 'traffic.filteredResultsStale']) {
    assert.match(page, new RegExp(`t\\('${key}'\\)`));
    assert.equal(i18n.match(new RegExp(`'${key}': '`, 'g'))?.length, 2);
  }
  // No locale-ternary hardcoded pause copy remains on the page.
  assert.doesNotMatch(page, /'暂停更新'|'恢复更新'/);
});
