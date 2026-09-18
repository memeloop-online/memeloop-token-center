import assert from 'node:assert/strict';
import test from 'node:test';

import {
  REQUEST_EVENT_RECONNECT_INITIAL_DELAY_MS,
  REQUEST_EVENT_RECONNECT_JITTER_RATIO,
  REQUEST_EVENT_RECONNECT_MAX_DELAY_MS,
  requestEventReconnectDelayMs,
} from '../src/operator/hooks/requestEventStreamBackoff.js';

test('request-event reconnect delay grows exponentially and stays bounded', () => {
  const delays = Array.from({ length: 8 }, (_, attempt) => requestEventReconnectDelayMs(attempt, 0.5));
  assert.deepEqual(delays, [
    REQUEST_EVENT_RECONNECT_INITIAL_DELAY_MS,
    2_000,
    4_000,
    8_000,
    16_000,
    REQUEST_EVENT_RECONNECT_MAX_DELAY_MS,
    REQUEST_EVENT_RECONNECT_MAX_DELAY_MS,
    REQUEST_EVENT_RECONNECT_MAX_DELAY_MS,
  ]);
});

test('jitter is deterministic for a supplied sample and cannot exceed the cap', () => {
  const initial = REQUEST_EVENT_RECONNECT_INITIAL_DELAY_MS;
  const ratio = REQUEST_EVENT_RECONNECT_JITTER_RATIO;
  assert.equal(requestEventReconnectDelayMs(0, 0), Math.round(initial * (1 - ratio)));
  assert.equal(requestEventReconnectDelayMs(0, 1), Math.round(initial * (1 + ratio)));
  assert.equal(requestEventReconnectDelayMs(5, 1), REQUEST_EVENT_RECONNECT_MAX_DELAY_MS);
  assert.ok(requestEventReconnectDelayMs(10, 0) >= 0);
  assert.ok(requestEventReconnectDelayMs(10, 1) <= REQUEST_EVENT_RECONNECT_MAX_DELAY_MS);
});

test('invalid backoff inputs use safe finite defaults', () => {
  assert.equal(requestEventReconnectDelayMs(Number.NaN, Number.NaN), REQUEST_EVENT_RECONNECT_INITIAL_DELAY_MS);
  assert.equal(requestEventReconnectDelayMs(-4, -1), Math.round(REQUEST_EVENT_RECONNECT_INITIAL_DELAY_MS * (1 - REQUEST_EVENT_RECONNECT_JITTER_RATIO)));
  assert.equal(requestEventReconnectDelayMs(Number.POSITIVE_INFINITY, Number.POSITIVE_INFINITY), REQUEST_EVENT_RECONNECT_INITIAL_DELAY_MS);
});
