import assert from 'node:assert/strict';
import test from 'node:test';
import { mergeLiveRequestEvents, requestViewFromEvent } from '../src/operator/traffic/requestTraffic.js';
import type { RequestEvent, RequestView } from '../src/types.js';

const event: RequestEvent = {
  event_id: 'event', request_id: 'request', event_at: 900, event_kind: 'finished',
  key_id: 'key', protocol: 'openai', model: 'model', status_code: 200,
  duration_ms: 400, input_tokens: 100, output_tokens: 20, cost: '1', error_code: null,
};
const recorded: RequestView = {
  request_id: 'request', created_at: 100, completed_at: 800,
  protocol: 'openai', model: 'model', status_code: 200, duration_ms: 400,
  input_tokens: 100, output_tokens: 20, cost: '1', error_code: null,
  upstream_account_id: 'upstream', route_id: 'route', currency: 'USD',
  cached_input_tokens: 30, cache_write_tokens: 10,
  session_context: {
    session_id: 'session', association: 'confirmed', session_name: 'named',
    task_kind: 'task', agent_id: 'agent', semantics_source: 'declared',
  },
};

test('legacy SSE does not erase recorded routing, completion, currency, cache or session fields', () => {
  assert.deepEqual(requestViewFromEvent(event, recorded), recorded);
  const start: RequestEvent = { ...event, event_kind: 'started', status_code: null };
  assert.deepEqual(requestViewFromEvent(start, recorded), recorded);
});

test('finished-only SSE uses receipt and completion facts, never its event publication time', () => {
  const complete: RequestEvent = { ...event, ...recorded };
  assert.deepEqual(requestViewFromEvent(complete), recorded);
  assert.deepEqual(mergeLiveRequestEvents([], new Map([['request', complete]])), [recorded]);
  assert.equal(requestViewFromEvent(event), undefined);
});

test('missing cache telemetry remains absent instead of becoming zero', () => {
  const legacyStart = { ...event, event_kind: 'started' as const, status_code: null };
  const pending = requestViewFromEvent(legacyStart);
  assert.equal(pending?.created_at, event.event_at);
  assert.equal(pending?.cached_input_tokens, undefined);
  assert.equal(pending?.cache_write_tokens, undefined);
  assert.equal(pending?.completed_at, undefined);
  const terminal = requestViewFromEvent({ ...event, cached_input_tokens: 0, cache_write_tokens: 0 }, pending);
  assert.equal(terminal?.cached_input_tokens, 0);
  assert.equal(terminal?.cache_write_tokens, 0);
});
