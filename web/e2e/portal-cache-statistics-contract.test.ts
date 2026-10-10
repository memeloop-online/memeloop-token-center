import assert from 'node:assert/strict';
import test from 'node:test';
import { cacheStatisticsCopy, cacheStatisticsPresentation } from '../src/self/cacheStatistics.js';
import type { CacheStats } from '../src/types.js';

const known = (read: number, input: number, unknown = 0): CacheStats => ({ reported_read_tokens: read, reported_requests: 1, known_read_tokens: read, known_input_tokens: input, eligible_requests: 1, unknown_requests: unknown, hit_rate: input > 0 ? read / input : null });

test('Portal cache statistics distinguish unknown, observed zero, partial and denominator boundaries', () => {
  assert.deepEqual(cacheStatisticsPresentation(undefined), { tokens: null, rate: null, note: 'unavailable' });
  assert.equal(cacheStatisticsPresentation({ ...known(0, 0), reported_requests: 0, eligible_requests: 0, unknown_requests: 7 }).tokens, null);
  assert.deepEqual(cacheStatisticsPresentation({ ...known(0, 0), reported_read_tokens: 30, eligible_requests: 0, unknown_requests: 1 }), { tokens: 30, rate: null, note: 'unavailable' });
  assert.deepEqual(cacheStatisticsPresentation(known(0, 100)), { tokens: 0, rate: 0, note: 'definition' });
  assert.deepEqual(cacheStatisticsPresentation(known(40, 100, 2)), { tokens: 40, rate: 0.4, note: 'partial' });
  assert.deepEqual(cacheStatisticsPresentation(known(0, 0)), { tokens: 0, rate: null, note: 'noInput' });
  assert.equal(cacheStatisticsPresentation(known(100, 100)).rate, 1);
  assert.equal(cacheStatisticsPresentation(known(101, 100)).rate, null);
  assert.equal(cacheStatisticsPresentation({ ...known(40, 100), hit_rate: 0.2 }).rate, null);
  assert.equal(cacheStatisticsPresentation(known(NaN, 100)).tokens, null);
  for (const locale of ['zh-CN', 'en'] as const) {
    assert.ok(cacheStatisticsCopy[locale].unavailable.length > 20);
    assert.ok(cacheStatisticsCopy[locale].partial.length > 20);
    assert.ok(cacheStatisticsCopy[locale].noInput.length > 15);
  }
});
