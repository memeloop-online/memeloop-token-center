import assert from 'node:assert/strict';
import test from 'node:test';
import { analyticsAge, analyticsDuration, finiteP95Points, histogramP95, metricArea } from '../src/operator/analyticsPresentation.js';
import { formatCurrencyDisplay, formatMetricDisplay } from '../src/format.js';
import type { UsageAnalysisTimeBucket } from '../src/types.js';

test('analytics preserves precision while presenting compact human units', () => {
  assert.deepEqual(analyticsDuration(120_317.25, 'zh-CN'), { text: '2分0秒', title: '120,317.25 ms' });
  assert.equal(analyticsAge(8_000, 'zh-CN'), '8秒前');
  assert.equal(analyticsAge(0, 'en'), 'Just now');
  assert.deepEqual(formatCurrencyDisplay('6199.248234', 'USD', 'en'), { text: '$6,199.25', title: '$6,199.248234' });
  assert.equal(formatMetricDisplay(100_000_000, 'zh-CN').text, '1亿');
});

test('finite, overflow and legacy P95 buckets are never conflated', () => {
  assert.equal(histogramP95(60_000, true, 'zh-CN').text, '>1分钟');
  assert.equal(histogramP95(60_000, false, 'zh-CN').text, '≤1分0秒');
  assert.equal(histogramP95(60_000, undefined, 'zh-CN').text, '最高统计档');
  assert.equal(histogramP95(null, false, 'en').text, '—');
  const points = [false, true, undefined].map((capped) => ({ p95_duration_ms: 60_000, p95_is_capped: capped } as UsageAnalysisTimeBucket));
  assert.deepEqual(finiteP95Points(points).map((point) => point.p95_duration_ms), [60_000, null, null]);
  assert.equal(points[1].p95_duration_ms, 60_000, 'presentation must not alter authoritative data');
});

test('micro areas encode real variation, zero baselines, and missing-value gaps only', () => {
  assert.equal(metricArea([]), undefined);
  assert.equal(metricArea([0, 0, 0]), undefined);
  assert.equal(metricArea([10]), undefined);
  assert.notEqual(metricArea([1, 3, 2]), metricArea([3, 1, 2]));
  assert.equal((metricArea([1, 2, null, 2, 1])?.match(/M/g) ?? []).length, 2);
});
