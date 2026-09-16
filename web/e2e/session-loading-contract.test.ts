import assert from 'node:assert/strict';
import test from 'node:test';
import { defaultRequestRefreshInterval } from '../src/operator/traffic/requestRefresh.js';
import { sessionEventRefreshDelayMs } from '../src/operator/sessionRefresh.js';

test('session invalidation remains prompt without changing the request-table cadence', () => {
  assert.ok(sessionEventRefreshDelayMs <= 500, 'terminal session invalidation must settle within 500ms');
  assert.equal(defaultRequestRefreshInterval, 5_000, 'request-table rendering retains its default 5-second cadence');
});
