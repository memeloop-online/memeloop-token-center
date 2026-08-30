import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import type { UsageAnalysisHeatmapBucket, UsageAnalysisTimeBucket } from '../src/types.js';
import {
  costCurrencies,
  costOption,
  heatmapValue,
  latencyOption,
  throughputOption,
  totalTokens,
  type UsageChartCopy,
  type UsageChartFormatters,
} from '../src/charts/usageCharts.js';

const copy: UsageChartCopy = {
  requests: 'Requests', success: 'Success', failures: 'Failed', averageLatency: 'Average',
  p95Latency: 'P95 (approx.)', cost: 'Cost', noData: 'No data',
};
const format: UsageChartFormatters = {
  bucket: String, cost: (value, currency) => `${currency} ${value}`, duration: (value) => `${value}ms`,
  number: String, percent: (value) => `${value * 100}%`,
};
const point: UsageAnalysisTimeBucket = {
  bucket_start: 1, requests: 6, success: 4, failed: 2, input_tokens: 10, output_tokens: 5,
  cached_input_tokens: 3, cache_write_tokens: 2, generation_units: 0, avg_duration_ms: 20,
  p95_duration_ms: 70, costs: [{ currency: 'USD', cost: '1.25' }, { currency: 'CNY', cost: '4.5' }],
};

test('throughput and latency stay in distinct, truthful series', () => {
  const throughput = throughputOption([point], copy, format) as { series: Array<{ name: string; data: number[]; stack?: string }> };
  assert.deepEqual(throughput.series.map((series) => [series.name, series.data]), [['Success', [4]], ['Failed', [2]]]);
  assert.ok(throughput.series.every((series) => series.stack === 'requests'));
  const latency = latencyOption([point], copy, format) as { series: Array<{ name: string; data: Array<number | null> }> };
  assert.deepEqual(latency.series.map((series) => [series.name, series.data]), [['Average', [20]], ['P95 (approx.)', [70]]]);
});

test('cost charts never add unlike currencies', () => {
  assert.deepEqual(costCurrencies([point]), ['CNY', 'USD']);
  const option = costOption([point], copy, format) as { series: Array<{ name: string; data: Array<number | null>; tooltip: { valueFormatter: (value: unknown) => string } }> };
  assert.deepEqual(option.series.map((series) => [series.name, series.data]), [['Cost · CNY', [4.5]], ['Cost · USD', [1.25]]]);
  assert.deepEqual(option.series.map((series) => series.tooltip.valueFormatter(series.data[0])), ['CNY 4.5', 'USD 1.25']);
  assert.ok(!JSON.stringify(option).includes('5.75'));
});

test('heatmap supports requests, tokens, currency cost, and failure-rate drill values', () => {
  const cell: UsageAnalysisHeatmapBucket = { ...point, hour_of_week: 9 };
  assert.equal(totalTokens(cell), 20);
  assert.equal(heatmapValue(cell, 'requests', ''), 6);
  assert.equal(heatmapValue(cell, 'tokens', ''), 20);
  assert.equal(heatmapValue(cell, 'cost', 'USD'), 1.25);
  assert.equal(heatmapValue(cell, 'failure_rate', ''), 1 / 3);
});

test('adapter is core-only, accessible, responsive, and disposes its canvas', async () => {
  const source = await readFile(new URL('../src/charts/EChart.tsx', import.meta.url), 'utf8');
  assert.match(source, /from 'echarts\/core'/);
  assert.match(source, /CanvasRenderer/);
  assert.match(source, /AriaComponent/);
  assert.match(source, /new ResizeObserver/);
  assert.match(source, /chart\.dispose\(\)/);
  assert.match(source, /role="img"/);
  assert.match(source, /chart\.setOption\(optionRef\.current/);
  assert.doesNotMatch(source, /\[locale, theme, timeZone\]/);
});

test('heatmap exposes a keyboard-operable equivalent data table', async () => {
  const source = await readFile(new URL('../src/charts/HeatmapDataTable.tsx', import.meta.url), 'utf8');
  assert.match(source, /<table>/);
  assert.match(source, /<button type="button"/);
  assert.match(source, /scope="row"/);
});
