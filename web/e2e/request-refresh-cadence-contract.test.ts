import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import type { RequestEvent } from '../src/types.js';
import { coalesceRequestEvent, defaultRequestRefreshInterval, mergeBatchedRequestPage, RequestRefreshBatch, requestRefreshIntervals, requestRefreshPreference } from '../src/operator/traffic/requestRefresh.js';
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

test('default and persisted cadence allow exactly the five requested positions', () => {
  assert.equal(defaultRequestRefreshInterval, 5000);
  assert.deepEqual(requestRefreshIntervals, [0, 5000, 30000, 60000, 300000]);
  for (const interval of requestRefreshIntervals) assert.equal(requestRefreshPreference(String(interval)), interval);
  for (const invalid of [null, 'bad', '-1', '1000']) assert.equal(requestRefreshPreference(invalid), 5000);
});

test('bursts publish one batch per selected cadence, never one render per event', () => {
  for (const interval of requestRefreshIntervals) {
    const time = clock(); const batches: Map<string, RequestEvent>[] = [];
    const batch = new RequestRefreshBatch(interval, time, values => batches.push(values));
    for (let i = 1; i <= 1000; i++) batch.enqueue(event('request', i));
    assert.equal(time.size, 1); assert.equal(batches.length, 0);
    time.advance(Math.max(16, interval) - 1); assert.equal(batches.length, 0);
    batch.enqueue(event('request', 1001, 'finished'));
    time.advance(1); assert.equal(batches.length, 1);
    assert.equal(batches[0].get('request')?.status_code, 200);
    assert.equal(batches[0].get('request')?.output_tokens, 2);
    batch.dispose();
  }
});

test('later partial archive updates and replayed starts cannot erase a batched terminal', () => {
  const finished = event('request', 20, 'finished');
  const archive = coalesceRequestEvent(finished, event('request', 21, 'archive_bound'));
  assert.equal(archive.status_code, 200); assert.equal(archive.completed_at, 20);
  assert.equal(archive.output_tokens, 2);
  assert.equal(coalesceRequestEvent(archive, event('request', 19)), archive);
});

test('hidden tabs and cadence changes retain pending terminals, dispose cancels all work', () => {
  const time = clock(); const values: RequestEvent[] = [];
  const batch = new RequestRefreshBatch(5000, time, events => values.push(...events.values()));
  batch.enqueue(event('a', 1)); batch.setPaused(true);
  batch.enqueue(event('a', 2, 'finished')); time.advance(300000); assert.equal(values.length, 0);
  batch.setInterval(30000); batch.setPaused(false); time.advance(29999); assert.equal(values.length, 0);
  time.advance(1); assert.equal(values[0].status_code, 200);
  batch.enqueue(event('b', 3)); batch.setInterval(0); time.advance(16); assert.equal(values.length, 2);
  batch.enqueue(event('c', 4)); batch.dispose(); time.advance(300000); assert.equal(values.length, 2);
  assert.equal(time.size, 0); batch.enqueue(event('d', 5)); assert.equal(time.size, 0);
});

test('bounded overflow protects visible terminal records and requests authoritative reconciliation', () => {
  const time = clock(); let received = new Map<string, RequestEvent>(); let overflow = false;
  const batch = new RequestRefreshBatch(5000, time, (events, lost) => { received = events; overflow = lost; }, 2);
  batch.protect(['visible']); batch.enqueue(event('visible', 1, 'finished'));
  for (let i = 2; i < 100; i++) batch.enqueue(event(String(i), i));
  time.advance(5000); assert.equal(received.size, 3); assert.equal(overflow, true);
  assert.equal(received.get('visible')?.status_code, 200);
});

test('automatic live traffic stays bounded and restores history navigation after displacement', () => {
  const snapshot = [requestViewFromEvent(event('oldest', 1, 'finished'))!];
  const events = new Map(Array.from({ length: 500 }, (_, i) => [`live-${i}`, { ...event(`live-${i}`, i + 2, 'finished'), created_at: i + 2 }]));
  const page = mergeBatchedRequestPage(snapshot, events, false);
  assert.equal(page.requests.length, 100); assert.equal(page.hasOlder, true);
  const loaded = Array.from({ length: 250 }, (_, i) => requestViewFromEvent({ ...event(`history-${i}`, 1, 'finished'), created_at: -i })!);
  const historyIds = new Set(loaded.slice(100).map(request => request.request_id));
  const retained = mergeBatchedRequestPage(loaded, events, true, historyIds);
  assert.equal(retained.requests.length, 250);
  assert.deepEqual(retained.requests.map(request => request.request_id), loaded.map(request => request.request_id));
  for (const id of historyIds) assert.ok(retained.requests.some(request => request.request_id === id));
});

test('React wiring batches SSE revisions and publishes list and metrics from the same request snapshot', async () => {
  const stream = await readFile(new URL('../src/operator/hooks/useRequestEventStream.ts', import.meta.url), 'utf8');
  const operator = await readFile(new URL('../src/operator/hooks/useOperatorRequestStream.ts', import.meta.url), 'utf8');
  const page = await readFile(new URL('../src/operator/pages/RequestsPage.tsx', import.meta.url), 'utf8');
  const preference = await readFile(new URL('../src/operator/hooks/useRequestRefreshPreference.ts', import.meta.url), 'utf8');
  assert.doesNotMatch(stream.slice(stream.indexOf('({ id, event:'), stream.indexOf('connectedOnce = true')), /dispatch\(\{ type: 'live' \}\)/);
  assert.match(operator, /batch\.current\?\.enqueue\(event\)/);
  assert.match(operator, /enabled: enabled && !paused/);
  assert.match(operator, /next\.dispose\(\)/);
  assert.match(page, /summarizeVisibleRequests\(requests\)/);
  assert.match(page, /RequestTable requests=\{requests\}/);
  assert.match(page, /requests\.map\(request => request\.request_id\)/);
  assert.match(preference, /visibilitychange/); assert.match(preference, /removeEventListener/);
  assert.match(preference, /localStorage\.setItem/);
});
