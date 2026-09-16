import assert from 'node:assert/strict';
import test from 'node:test';
import { averageBucketTps, averageBucketTpsSeries, averageSeriesTps, analyticsAge, analyticsDuration, finiteP95Points, formatTps, histogramP95, metricArea } from '../src/operator/analyticsPresentation.js';
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
  assert.equal(histogramP95(-1, false, 'zh-CN').text, '—');
  const points = [false, true, undefined].map((capped) => ({ p95_duration_ms: 60_000, p95_is_capped: capped } as UsageAnalysisTimeBucket));
  assert.deepEqual(finiteP95Points(points).map((point) => point.p95_duration_ms), [60_000, null, null]);
  assert.equal(points[1].p95_duration_ms, 60_000, 'presentation must not alter authoritative data');
});

test('micro areas encode real variation, zero baselines, and missing-value gaps only', () => {
  assert.equal(metricArea([]), undefined);
  assert.equal(metricArea([0, 0, 0]), undefined);
  assert.equal(metricArea([10]), undefined);
  assert.notEqual(metricArea([1, 3, 2]), metricArea([3, 1, 2]));
  assert.equal(metricArea([0, 1, 0]), 'M0,48 L0,48 L100,6 L200,48 L200,48 Z', 'zero values must sit on the closing baseline without fabricated area');
  assert.equal((metricArea([1, 2, null, 2, 1])?.match(/M/g) ?? []).length, 2);
});

test('TPS summaries and background series are derived from historical buckets', () => {
  const points = [
    { requests: 2, output_tokens: 200, avg_duration_ms: 200 } as UsageAnalysisTimeBucket,
    { requests: 4, output_tokens: 120, avg_duration_ms: 500 } as UsageAnalysisTimeBucket,
    { requests: 1, output_tokens: 60, avg_duration_ms: 1000 } as UsageAnalysisTimeBucket,
  ];
  assert.deepEqual(points.map((point) => averageBucketTps(point)), [500, 60, 60]);
  assert.equal(averageSeriesTps(points), 111.76470588235294);
  assert.deepEqual(averageBucketTpsSeries([...points, { requests: 1, output_tokens: 10, avg_duration_ms: null } as UsageAnalysisTimeBucket]), [500, 60, 60, null]);
  assert.equal(averageBucketTps({ requests: 2, output_tokens: 100, avg_duration_ms: 500 }), 100, 'one 1-second request plus a zero-duration failure: the backend counts both in the duration average');
  assert.deepEqual(formatTps(111.76470588235294, 'en'), { text: '111.76', title: '111.764706 TPS' });
});
