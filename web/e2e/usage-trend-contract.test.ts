import assert from 'node:assert/strict';
import test from 'node:test';
import type { UsageAnalysisTimeBucket } from '../src/types.js';
import { trendCurrencies, trendValue } from '../src/operator/usageTrend.js';

const point: UsageAnalysisTimeBucket = {
  bucket_start: 0,
  requests: 3,
  success: 2,
  failed: 1,
  input_tokens: 10,
  output_tokens: 4,
  cached_input_tokens: 3,
  cache_write_tokens: 2,
  generation_units: 0,
  avg_duration_ms: 12.5,
  p95_duration_ms: 50,
  costs: [
    { currency: 'USD', cost: '1.25' },
    { currency: 'CNY', cost: '2.5' },
  ],
};

test('trend metrics expose requests, complete token totals, and both latency projections', () => {
  assert.equal(trendValue(point, 'requests', ''), 3);
  assert.equal(trendValue(point, 'tokens', ''), 19);
  assert.equal(trendValue(point, 'avg_latency', ''), 12.5);
  assert.equal(trendValue(point, 'p95_latency', ''), 50);
});

test('cost trends select exactly one currency and never add unlike currencies', () => {
  assert.deepEqual(trendCurrencies([point]), ['CNY', 'USD']);
  assert.equal(trendValue(point, 'cost', 'CNY'), 2.5);
  assert.equal(trendValue(point, 'cost', 'USD'), 1.25);
  assert.equal(trendValue(point, 'cost', 'EUR'), 0);
  assert.notEqual(trendValue(point, 'cost', 'USD'), 3.75);
});

test('malformed decimal costs do not become chart coordinates', () => {
  assert.equal(trendValue({ ...point, costs: [{ currency: 'USD', cost: 'not-a-number' }] }, 'cost', 'USD'), null);
});
