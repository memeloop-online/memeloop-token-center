import assert from 'node:assert/strict';
import test from 'node:test';
import type { RequestEvent } from '../src/types.js';
import { SessionEventChannel } from '../src/operator/hooks/useOperatorRequestStream.js';

function event(requestId: string): RequestEvent {
  return {
    event_id: requestId, event_at: 1, event_kind: 'finished', request_id: requestId,
    key_id: 'key', created_at: 1, protocol: 'openai', model: 'model', status_code: 200,
    completed_at: 1, duration_ms: 1, input_tokens: 1, output_tokens: 1, cost: '0',
    error_code: null, archive_state: 'pending',
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
