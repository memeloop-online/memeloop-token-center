import assert from 'node:assert/strict';
import test from 'node:test';
import { defaultRequestRefreshInterval } from '../src/operator/traffic/requestRefresh.js';
import { sessionEventRefreshDelayMs, sessionRefreshDelayMs } from '../src/operator/sessionRefresh.js';

test('session invalidation follows the shared operator cadence and batches realtime bursts', () => {
  assert.equal(sessionRefreshDelayMs(0), sessionEventRefreshDelayMs, 'realtime still yields once to batch a burst');
  assert.equal(sessionRefreshDelayMs(5_000), 5_000);
  assert.equal(sessionRefreshDelayMs(30_000), 30_000);
  assert.equal(sessionRefreshDelayMs(60_000), 60_000);
  assert.equal(sessionRefreshDelayMs(300_000), 300_000);
  assert.equal(defaultRequestRefreshInterval, 5_000, 'requests and sessions share the default 5-second cadence');
});
