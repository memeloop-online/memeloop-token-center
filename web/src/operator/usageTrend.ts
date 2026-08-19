import type { UsageAnalysisTimeBucket } from '../types.js';

export type TrendMetric = 'requests' | 'tokens' | 'cost' | 'avg_latency' | 'p95_latency';

export const trendMetrics: readonly TrendMetric[] = [
  'requests', 'tokens', 'cost', 'avg_latency', 'p95_latency',
];

export function trendCurrencies(points: UsageAnalysisTimeBucket[]) {
  return [...new Set(points.flatMap((point) => point.costs.map((cost) => cost.currency)))].sort();
}

export function trendValue(point: UsageAnalysisTimeBucket, metric: TrendMetric, currency: string): number | null {
  if (metric === 'requests') return point.requests;
  if (metric === 'tokens') {
    return point.input_tokens + point.output_tokens + point.cached_input_tokens + point.cache_write_tokens;
  }
  if (metric === 'cost') {
    const cost = point.costs.find((candidate) => candidate.currency === currency)?.cost;
    if (cost === undefined) return 0;
    const numeric = Number(cost);
    return Number.isFinite(numeric) ? numeric : null;
  }
  return metric === 'avg_latency' ? point.avg_duration_ms : point.p95_duration_ms;
}
