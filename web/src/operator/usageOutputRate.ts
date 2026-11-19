import type { OperatorUsageAnalysis, UsageAnalysisOutputRate } from '../types';

/** Partial TPS responses contain placeholder totals; merge the rate alone. */
export function withUsageOutputRate(stats: OperatorUsageAnalysis, rates: OperatorUsageAnalysis): OperatorUsageAnalysis {
  const buckets = new Map(rates.time_series.map((point) => [point.bucket_start, point.output_rate]));
  const empty: UsageAnalysisOutputRate = { requests: 0, output_tokens: 0, duration_ms: 0 };
  return {
    ...stats,
    summary: { ...stats.summary, output_rate: rates.summary.output_rate },
    time_series: stats.time_series.map((point) => ({ ...point, output_rate: buckets.get(point.bucket_start) ?? empty })),
  };
}
