import assert from 'node:assert/strict';
import test from 'node:test';

import { summarizeVisibleRequests, visibleRequestMetricSeries } from '../src/operator/traffic/requestTraffic.js';
import type { RequestView } from '../src/types.js';

let requestNumber = 0;

test('metric backgrounds retain loaded-record scope and leave unknown latency gaps', () => {
  const points = visibleRequestMetricSeries([
    { ...request(200, 100), created_at: 1_000, completed_at: 1_100 },
    { ...request(502, 300), created_at: 1_007, completed_at: 1_307 },
    { ...request(null, null), created_at: 1_003 },
  ]);
  assert.equal(points.length, 8);
  assert.equal(points.reduce((sum, point) => sum + point.requests, 0), 3);
  assert.equal(points[0].successful, 1);
  assert.equal(points[7].failed, 1);
  assert.equal(points[3].running, 1);
  assert.equal(points[3].averageDurationMs, null);
  assert.equal(points[2].averageDurationMs, null);
  assert.deepEqual(visibleRequestMetricSeries([]), []);
  assert.equal(visibleRequestMetricSeries([request(200, 100), request(200, 200)]).length, 1);
});

function request(status_code: number | null, duration_ms: number | null): RequestView {
  return {
    request_id: `request-${++requestNumber}`, created_at: 1, protocol: 'openai', model: 'example', status_code, duration_ms,
    input_tokens: 0, output_tokens: 0, cost: '0', error_code: null, completed_at: status_code === null ? null : 400,
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
    unknown: 0,
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
    unknown: 0,
    successRate: null,
    averageDurationMs: null,
  });
  const unknown = summarizeVisibleRequests([{ ...request(200, 100), completed_at: undefined }]);
  assert.equal(unknown.unknown, 1);
  assert.equal(unknown.successful, 0);
  assert.equal(unknown.successRate, null);
});

test('finished-request rate includes disconnected and interrupted outcomes but not delivery or unknown history', () => {
  const summary = summarizeVisibleRequests([
    request(200, 100),
    { ...request(499, 100), error_code: 'client_cancelled' },
    { ...request(200, 100), error_code: 'upstream_incomplete_response' },
    { ...request(null, null), error_code: 'delivery_started' },
    { ...request(200, 100), completed_at: undefined },
  ]);
  assert.equal(summary.requests, 5);
  assert.equal(summary.successful, 1);
  assert.equal(summary.failed, 2);
  assert.equal(summary.running, 1);
  assert.equal(summary.unknown, 1);
  assert.equal(summary.successRate, 1 / 3);
});
