import type { EChartsCoreOption } from 'echarts/types/dist/core';
import type { UsageAnalysisHeatmapBucket, UsageAnalysisTimeBucket } from '../types.js';

export type HeatmapMetric = 'requests' | 'tokens' | 'cost' | 'failure_rate';

export interface UsageChartCopy {
  requests: string;
  success: string;
  failures: string;
  averageLatency: string;
  p95Latency: string;
  cost: string;
  noData: string;
}

export interface UsageChartFormatters {
  bucket: (epoch: number) => string;
  cost: (value: number, currency: string) => string;
  duration: (value: number) => string;
  number: (value: number) => string;
  percent: (value: number) => string;
}

export function totalTokens(value: Pick<UsageAnalysisTimeBucket, 'input_tokens' | 'output_tokens' | 'cached_input_tokens' | 'cache_write_tokens'>) {
  return value.input_tokens + value.output_tokens + value.cached_input_tokens + value.cache_write_tokens;
}

export function costCurrencies(values: Array<Pick<UsageAnalysisTimeBucket, 'costs'>>) {
  return [...new Set(values.flatMap((value) => value.costs.map((cost) => cost.currency)))].sort();
}

function numericCost(cost: string | undefined) {
  const value = Number(cost ?? 0);
  return Number.isFinite(value) ? value : null;
}

function baseOption(labels: string[], ariaLabel: string): EChartsCoreOption {
  return {
    animationDuration: 260,
    aria: { enabled: true, label: { description: ariaLabel } },
    dataZoom: labels.length > 48 ? [
      { type: 'inside', start: 70, end: 100 },
      { type: 'slider', height: 18, bottom: 4, start: 70, end: 100 },
    ] : undefined,
    grid: { containLabel: true, left: 8, right: 18, top: 48, bottom: labels.length > 48 ? 48 : 16 },
    legend: { top: 4 },
    tooltip: { trigger: 'axis', confine: true },
    xAxis: { type: 'category', boundaryGap: true, data: labels, axisLabel: { hideOverlap: true } },
    yAxis: { type: 'value', minInterval: 1 },
  };
}

export function throughputOption(points: UsageAnalysisTimeBucket[], copy: UsageChartCopy, format: UsageChartFormatters): EChartsCoreOption {
  const labels = points.map((point) => format.bucket(point.bucket_start));
  return {
    ...baseOption(labels, `${copy.requests}: ${copy.success}, ${copy.failures}`),
    tooltip: { trigger: 'axis', confine: true, valueFormatter: (value: unknown) => format.number(Number(value)) },
    series: [
      { name: copy.success, type: 'bar', stack: 'requests', barMaxWidth: 24, data: points.map((point) => point.success) },
      { name: copy.failures, type: 'bar', stack: 'requests', barMaxWidth: 24, data: points.map((point) => point.failed) },
    ],
  };
}

export function latencyOption(points: UsageAnalysisTimeBucket[], copy: UsageChartCopy, format: UsageChartFormatters): EChartsCoreOption {
  const labels = points.map((point) => format.bucket(point.bucket_start));
  return {
    ...baseOption(labels, `${copy.averageLatency}, ${copy.p95Latency}`),
    tooltip: { trigger: 'axis', confine: true, valueFormatter: (value: unknown) => format.duration(Number(value)) },
    yAxis: { type: 'value', axisLabel: { formatter: (value: number) => format.duration(value) } },
    series: [
      { name: copy.averageLatency, type: 'line', connectNulls: false, showSymbol: points.length < 32, smooth: 0.18, data: points.map((point) => point.avg_duration_ms) },
      { name: copy.p95Latency, type: 'line', connectNulls: false, showSymbol: points.length < 32, smooth: 0.18, data: points.map((point) => point.p95_duration_ms) },
    ],
  };
}

export function costOption(points: UsageAnalysisTimeBucket[], copy: UsageChartCopy, format: UsageChartFormatters): EChartsCoreOption {
  const labels = points.map((point) => format.bucket(point.bucket_start));
  const currencies = costCurrencies(points);
  return {
    ...baseOption(labels, copy.cost),
    tooltip: { trigger: 'axis', confine: true },
    yAxis: { type: 'value' },
    series: currencies.map((currency) => ({
      name: `${copy.cost} · ${currency}`,
      type: 'line',
      showSymbol: points.length < 32,
      smooth: 0.18,
      tooltip: { valueFormatter: (value: unknown) => format.cost(Number(value), currency) },
      data: points.map((point) => numericCost(point.costs.find((candidate) => candidate.currency === currency)?.cost)),
    })),
  };
}

export function heatmapValue(value: UsageAnalysisHeatmapBucket, metric: HeatmapMetric, currency: string) {
  if (metric === 'requests') return value.requests;
  if (metric === 'tokens') return totalTokens(value);
  if (metric === 'failure_rate') return value.requests > 0 ? value.failed / value.requests : 0;
  return numericCost(value.costs.find((candidate) => candidate.currency === currency)?.cost) ?? 0;
}

export function heatmapOption(
  values: UsageAnalysisHeatmapBucket[],
  metric: HeatmapMetric,
  currency: string,
  weekdays: string[],
  ariaLabel: string,
  format: UsageChartFormatters,
): EChartsCoreOption {
  const cells = values.map((value) => [value.hour_of_week % 24, Math.floor(value.hour_of_week / 24), heatmapValue(value, metric, currency)]);
  const maximum = Math.max(1, ...cells.map((cell) => Number(cell[2])));
  const formatter = metric === 'failure_rate'
    ? format.percent
    : metric === 'cost'
      ? (value: number) => format.cost(value, currency)
      : format.number;
  return {
    aria: { enabled: true, label: { description: ariaLabel } },
    grid: { containLabel: true, left: 8, right: 20, top: 12, bottom: 40 },
    tooltip: {
      position: 'top',
      confine: true,
      formatter: (params: unknown) => {
        const candidate = Array.isArray(params) ? params[0] : params;
        const value = candidate && typeof candidate === 'object' && 'value' in candidate
          ? (candidate as { value?: unknown }).value
          : undefined;
        if (!Array.isArray(value) || value.length < 3) return '';
        const hour = Number(value[0]);
        const weekday = Number(value[1]);
        return `${weekdays[weekday] ?? ''} ${String(hour).padStart(2, '0')}:00 · ${formatter(Number(value[2]))}`;
      },
    },
    visualMap: { min: 0, max: maximum, calculable: true, orient: 'horizontal', left: 'center', bottom: 0, formatter: (value: unknown) => formatter(Number(value)) },
    xAxis: { type: 'category', data: Array.from({ length: 24 }, (_, hour) => String(hour).padStart(2, '0')), splitArea: { show: true } },
    yAxis: { type: 'category', data: weekdays, splitArea: { show: true } },
    series: [{ type: 'heatmap', data: cells, emphasis: { itemStyle: { shadowBlur: 8, shadowColor: 'rgba(0,0,0,.35)' } } }],
  };
}
