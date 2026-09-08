import assert from 'node:assert/strict';
import test from 'node:test';

import { summarizeVisibleRequests } from '../src/operator/traffic/requestTraffic.js';
import type { RequestView } from '../src/types.js';

let requestNumber = 0;

function request(status_code: number | null, duration_ms: number | null): RequestView {
  return {
    request_id: `request-${++requestNumber}`, created_at: 1, protocol: 'openai', model: 'example', status_code, duration_ms,
    input_tokens: 0, output_tokens: 0, cost: '0', error_code: null,
  };
}

test('visible traffic summary separates terminal health from live work', () => {
  const summary = summarizeVisibleRequests([
    request(200, 120), request(429, 360), request(null, null), request(204, null),
  ]);

  assert.deepEqual(summary, {
    requests: 4,
    successful: 2,
    failed: 1,
    running: 1,
    successRate: 2 / 3,
    averageDurationMs: 240,
  });
});

test('visible traffic summary has no fabricated rate or latency without terminal evidence', () => {
  assert.deepEqual(summarizeVisibleRequests([request(null, null)]), {
    requests: 1,
    successful: 0,
    failed: 0,
    running: 1,
    successRate: null,
    averageDurationMs: null,
  });
});
