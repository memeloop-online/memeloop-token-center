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
    cacheRate: null,
    averageDurationMs: 240,
    totalTokens: 0,
    localCosts: [],
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
    cacheRate: null,
    averageDurationMs: null,
    totalTokens: 0,
    localCosts: [],
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

test('totalTokens counts actual usage only: pending and non-actual history never count', () => {
  const nonActual = (status_code: number, usage_basis: RequestView['usage_basis']): RequestView => ({
    ...request(status_code, 100), input_tokens: 1_000, output_tokens: 2_000, usage_basis,
  });
  const failedProviderReported: RequestView = { ...request(500, 100), input_tokens: 20, output_tokens: 3, usage_basis: 'provider_reported' };
  const summary = summarizeVisibleRequests([
    { ...request(200, 100), input_tokens: 10, output_tokens: 2, usage_basis: 'provider_reported' },
    failedProviderReported,
    { ...request(null, null), input_tokens: 5_000, output_tokens: 6_000, usage_basis: 'provider_reported' },
    nonActual(200, 'provider_estimated'),
    nonActual(200, 'contract_ceiling'),
    nonActual(502, 'not_observed'),
    nonActual(502, undefined),
    nonActual(502, null),
  ]);
  assert.equal(summary.failed, 4);
  assert.equal(summary.running, 1);
  assert.equal(summary.totalTokens, 35);
  // Failed provider-reported usage is actual and remains included.
  assert.equal(summarizeVisibleRequests([failedProviderReported]).totalTokens, 23);
});

test('cache rate uses only actual requests with complete cache telemetry', () => {
  const summary = summarizeVisibleRequests([
    { ...request(200, 100), input_tokens: 100, cached_input_tokens: 20, cache_write_tokens: 10 },
    { ...request(200, 100), input_tokens: 50, cached_input_tokens: 0, cache_write_tokens: 0 },
    { ...request(200, 100), input_tokens: 500 },
    { ...request(null, null), input_tokens: 500, cached_input_tokens: 500, cache_write_tokens: 0 },
  ]);
  assert.equal(summary.cacheRate, 20 / 150);
});

test('local cost totals follow the shared displayed-cost policy per currency', () => {
  // Every failed request without provider-reported usage uses the policy zero,
  // including historic estimated, ceiling and unknown provenance rows.
  const zeroCostFailures: RequestView[] = ([
    ['not_observed', 499, 'client_cancelled'],
    ['provider_estimated', 502, null],
    ['contract_ceiling', 503, null],
    [undefined, 200, 'upstream_incomplete_response'],
  ] as const).map(([usage_basis, status_code, error_code], index) => ({
    ...request(status_code, 100), usage_basis, error_code,
    cost: String(index + 1), currency: 'USD',
  }));
  assert.deepEqual(summarizeVisibleRequests(zeroCostFailures).localCosts, [
    { currency: 'USD', cost: 0 },
  ]);

  // A failed provider-reported request keeps its nonzero local settlement.
  const failedProviderReported: RequestView = {
    ...request(502, 100), usage_basis: 'provider_reported', cost: '1.5', currency: 'USD',
  };
  assert.deepEqual(summarizeVisibleRequests([failedProviderReported]).localCosts, [
    { currency: 'USD', cost: 1.5 },
  ]);

  // Totals stay grouped per recorded currency; amounts without a recorded
  // currency never contribute and currencies are never combined.
  const summary = summarizeVisibleRequests([
    ...zeroCostFailures,
    failedProviderReported,
    { ...request(200, 100), usage_basis: 'provider_reported', cost: '2', currency: 'CNY' },
    { ...request(200, 100), usage_basis: 'provider_reported', cost: '9.99' },
  ]);
  assert.deepEqual(summary.localCosts, [
    { currency: 'CNY', cost: 2 },
    { currency: 'USD', cost: 1.5 },
  ]);
});
