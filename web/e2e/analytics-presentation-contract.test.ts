import assert from 'node:assert/strict';
import test from 'node:test';
import { averageBucketTps, averageBucketTpsSeries, averageSeriesTps, analyticsAge, analyticsDuration, finiteP95Points, formatTps, histogramP95, metricArea, seriesTpsSamples } from '../src/operator/analyticsPresentation.js';
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

test('TPS summaries and background series derive only from eligible output_rate samples', () => {
  const points = [
    { output_rate: { requests: 2, output_tokens: 200, duration_ms: 400 }, requests: 99, output_tokens: 9_999, avg_duration_ms: 1 } as UsageAnalysisTimeBucket,
    { output_rate: { requests: 4, output_tokens: 120, duration_ms: 2_000 }, requests: 7, output_tokens: 5_000, avg_duration_ms: 60_000 } as UsageAnalysisTimeBucket,
    { output_rate: { requests: 1, output_tokens: 60, duration_ms: 1_000 } } as UsageAnalysisTimeBucket,
  ];
  assert.deepEqual(points.map((point) => averageBucketTps(point)), [500, 60, 60], 'total request, token and duration counts must not contaminate the eligible rate');
  assert.equal(averageSeriesTps(points), 111.76470588235294, 'summary pools eligible numerators and denominators, not total counts');
  assert.equal(seriesTpsSamples(points), 7);
  const legacy = { requests: 2, output_tokens: 100, avg_duration_ms: 500 } as UsageAnalysisTimeBucket;
  assert.equal(averageBucketTps(legacy), null, 'missing output_rate never falls back to legacy totals');
  assert.equal(averageSeriesTps([legacy]), null, 'a series without provenance stays unavailable, not zero');
  assert.equal(seriesTpsSamples([legacy]), null, 'unknown provenance never reports a fabricated zero sample count');
  assert.deepEqual(averageBucketTpsSeries([...points, legacy]), [500, 60, 60, null], 'buckets without provenance remain real gaps');
  assert.equal(averageBucketTps({ output_rate: { requests: 0, output_tokens: 100, duration_ms: 500 } } as UsageAnalysisTimeBucket), null, 'zero eligible samples render an em dash');
  assert.equal(averageSeriesTps([{ output_rate: { requests: 0, output_tokens: 0, duration_ms: 0 } } as UsageAnalysisTimeBucket]), null);
  assert.equal(seriesTpsSamples([{ output_rate: { requests: 0, output_tokens: 0, duration_ms: 0 } } as UsageAnalysisTimeBucket]), 0, 'known zero samples differ from unknown provenance');
  assert.deepEqual(formatTps(111.76470588235294, 'en'), { text: '111.76', title: '111.764706 TPS' });
  assert.deepEqual(formatTps(null, 'en'), { text: '—' });
});
