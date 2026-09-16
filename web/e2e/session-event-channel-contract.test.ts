import assert from 'node:assert/strict';
import test from 'node:test';
import type { RequestEvent } from '../src/types.js';
import { SessionEventChannel } from '../src/operator/hooks/useOperatorRequestStream.js';
import { RequestRefreshBatch } from '../src/operator/traffic/requestRefresh.js';

function event(requestId: string): RequestEvent {
  return {
    event_id: requestId, event_at: 1, event_kind: 'finished', request_id: requestId,
    key_id: 'key', created_at: 1, protocol: 'openai', model: 'model', status_code: 200,
    completed_at: 1, duration_ms: 1, input_tokens: 1, output_tokens: 1, cost: '0',
    error_code: null, archive_state: 'pending',
  };
}

function fakeClock() {
  let now = 0; let id = 0;
  const timers = new Map<number, { at: number; run: () => void }>();
  return {
    schedule(run: () => void, delay: number) { timers.set(++id, { at: now + delay, run }); return id; },
    cancel(timer: number) { timers.delete(timer); },
    advance(milliseconds: number) {
      now += milliseconds;
      for (const [timerId, timer] of [...timers]) if (timer.at <= now) { timers.delete(timerId); timer.run(); }
    },
  };
}

test('session event channel drops unobserved SSE and isolates request-route rendering', () => {
  const channel = new SessionEventChannel();
  for (let index = 0; index < 1_000; index += 1) channel.publish(event(`unmounted-${index}`));
  assert.equal(channel.snapshot(), 0);
  assert.equal(channel.eventKeyIds.current.size, 0);

  let renders = 0;
  const unsubscribe = channel.subscribe(() => { renders += 1; });
  channel.publish(event('mounted'));
  assert.equal(renders, 1);
  assert.equal(channel.eventKeyIds.current.size, 1);
  unsubscribe();
  channel.clear();
  channel.publish(event('after-unmount'));
  assert.equal(renders, 1);
  assert.equal(channel.eventKeyIds.current.size, 0);
});

test('a high-frequency fake SSE stream does not render Requests outside its five-second batch', () => {
  const sessionChannel = new SessionEventChannel();
  const time = fakeClock();
  let sessionRenders = 0;
  let requestRenders = 0;
  const unmountSessions = sessionChannel.subscribe(() => { sessionRenders += 1; });
  const requestBatch = new RequestRefreshBatch(5_000, time, () => { requestRenders += 1; });
  const streamEvent = (value: RequestEvent) => {
    // This is the same split fan-out as useOperatorRequestStream: Sessions
    // sees the prompt channel; Requests only sees the five-second batch.
    sessionChannel.publish(value);
    requestBatch.enqueue(value);
  };

  for (let index = 0; index < 1_000; index += 1) streamEvent(event(`burst-${index}`));
  assert.equal(sessionRenders, 1_000);
  assert.equal(requestRenders, 0, 'Requests has no per-SSE render subscription');
  time.advance(4_999);
  assert.equal(requestRenders, 0);
  time.advance(1);
  assert.equal(requestRenders, 1, 'all burst events coalesce into one Requests render');

  unmountSessions();
  sessionChannel.clear();
  streamEvent(event('after-session-unmount'));
  assert.equal(sessionChannel.eventKeyIds.current.size, 0, 'unmounted Sessions retains no stream queue');
  requestBatch.dispose();
});
