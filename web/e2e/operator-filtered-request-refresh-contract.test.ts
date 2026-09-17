import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

import {
  filteredRequestRefreshDebounceMs,
  filteredRequestRefreshDelay,
  filteredRequestRefreshMaxWaitMs,
  mergeRefreshedRequestPage,
} from '../src/operator/traffic/requestTraffic.js';
import type { RequestListResponse, RequestView } from '../src/types.js';

function request(request_id: string, created_at: number, status_code: number | null): RequestView {
  return {
    request_id, created_at, status_code, protocol: 'openai', model: 'example', duration_ms: null,
    input_tokens: 0, output_tokens: 0, cost: '0', error_code: null,
  };
}

test('an exact filtered refresh turns a visible pending request terminal and retains loaded older history', () => {
  const refreshed: RequestListResponse = {
    requests: [request('pending-now-finished', 30, 200), request('newer-match', 25, 500)],
    next_cursor: { before_created_at: 25, before_id: 'newer-match' },
  };
  const merged = mergeRefreshedRequestPage([
    request('no-longer-matches', 35, null),
    request('pending-now-finished', 30, null),
    request('older-loaded', 20, 200),
  ], refreshed, true);

  assert.deepEqual(merged.map((value) => [value.request_id, value.status_code]), [
    ['pending-now-finished', 200], ['newer-match', 500], ['older-loaded', 200],
  ]);
  assert.deepEqual(mergeRefreshedRequestPage([request('stale-first-page', 20, null)], refreshed), refreshed.requests);
});

test('continuous filtered events are debounced but bounded by a maximum wait', () => {
  assert.equal(filteredRequestRefreshDelay(1_000, 1_000), filteredRequestRefreshDebounceMs);
  assert.equal(filteredRequestRefreshDelay(2_000, 1_000), filteredRequestRefreshDebounceMs);
  assert.equal(filteredRequestRefreshDelay(2_490, 1_000), 10);
  assert.equal(filteredRequestRefreshDelay(2_600, 1_000), 0);
  assert.equal(filteredRequestRefreshMaxWaitMs, 1_500);
});

test('filtered refresh preserves only rows strictly before its cursor and drops history at exhaustion', () => {
  const current = [
    request('b', 20, null), request('a', 20, 200), request('older', 10, 200),
  ];
  const refreshed: RequestListResponse = {
    requests: [request('b', 20, 200)],
    next_cursor: { before_created_at: 20, before_id: 'b' },
  };
  assert.deepEqual(mergeRefreshedRequestPage(current, refreshed, true).map((value) => [value.request_id, value.status_code]), [
    ['b', 200], ['a', 200], ['older', 200],
  ]);
  assert.deepEqual(mergeRefreshedRequestPage(current, { ...refreshed, next_cursor: null }, true), refreshed.requests);
});

test('filtered views stay stable: published batches only mark them stale for a manual refresh', async () => {
  const source = await readFile(new URL('../src/operator/pages/RequestsPage.tsx', import.meta.url), 'utf8');

  // Semantic contract only: batches mark filtered results stale, the manual
  // refresh entry stays, and no filtered auto-refresh can be scheduled.
  assert.match(source, /setFilteredResultsStale\(true\)/);
  assert.match(source, /t\('traffic\.filteredResultsStale'\)/);
  assert.match(source, /onRefreshFilteredResults/);
  assert.doesNotMatch(source, /scheduleFilteredRefresh/);
  // The overflow reconcile is guarded away from filtered scopes.
  assert.match(source, /typedFiltersActive\(currentScope\.filters\) \|\| !reconcileOverflow\.current/);
});
