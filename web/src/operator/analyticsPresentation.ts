import { formatMilliseconds, formatNumber, type FormattedValue } from '../format.js';
import type { Locale } from '../i18n.js';
import type { UsageAnalysisTimeBucket } from '../types.js';

// Eligible samples are independently filtered successful terminal text requests
// with provider-reported usage and positive end-to-end duration, excluding known
// compaction. Archived records lack this provenance and older servers omit
// output_rate entirely: totals must never substitute for the rate, so unknown
// stays unavailable instead of falling back to output_tokens/avg_duration_ms.
export function averageBucketTps(point: Pick<UsageAnalysisTimeBucket, 'output_rate'>): number | null {
  const rate = point.output_rate;
  if (rate == null) return null;
  if (!Number.isFinite(rate.requests) || rate.requests <= 0) return null;
  if (!Number.isFinite(rate.output_tokens) || rate.output_tokens < 0) return null;
  if (!Number.isFinite(rate.duration_ms) || rate.duration_ms <= 0) return null;
  return rate.output_tokens / (rate.duration_ms / 1_000);
}

export function averageBucketTpsSeries(points: readonly Pick<UsageAnalysisTimeBucket, 'output_rate'>[]): Array<number | null> {
  return points.map((point) => averageBucketTps(point));
}

// Summary TPS pools eligible numerators and denominators across buckets; it is
// not a mean of per-bucket rates. Zero eligible samples means unavailable, and
// the API has no per-request TPS distribution: bucket rates cannot supply P95.
export function averageSeriesTps(points: readonly Pick<UsageAnalysisTimeBucket, 'output_rate'>[]): number | null {
  let outputTokens = 0;
  let durationMillis = 0;
  let samples = 0;
  for (const point of points) {
    const rate = point.output_rate;
    if (rate == null) continue;
    if (!Number.isFinite(rate.requests) || rate.requests <= 0) continue;
    if (!Number.isFinite(rate.output_tokens) || rate.output_tokens < 0) continue;
    if (!Number.isFinite(rate.duration_ms) || rate.duration_ms <= 0) continue;
    outputTokens += rate.output_tokens;
    durationMillis += rate.duration_ms;
    samples += rate.requests;
  }
  if (samples <= 0 || durationMillis <= 0) return null;
  return outputTokens / (durationMillis / 1_000);
}

/** Eligible sample count: null when no bucket carries rate provenance, otherwise the total (possibly zero). */
export function seriesTpsSamples(points: readonly Pick<UsageAnalysisTimeBucket, 'output_rate'>[]): number | null {
  let samples: number | null = null;
  for (const point of points) {
    const requests = point.output_rate?.requests;
    if (requests == null || !Number.isFinite(requests) || requests < 0) continue;
    samples = (samples ?? 0) + requests;
  }
  return samples;
}

export function formatTps(value: number | null | undefined, locale: Locale): FormattedValue {
  if (value == null || !Number.isFinite(value) || value < 0) return { text: '—' };
  const maximumFractionDigits = value >= 1_000 ? 0 : 2;
  const text = formatNumber(value, locale, maximumFractionDigits);
  const title = formatNumber(value, locale, 6);
  return { text, title: `${title} TPS` };
}

export function analyticsDuration(value: number | null | undefined, locale: Locale): FormattedValue {
  const title = formatMilliseconds(value, locale);
  if (value == null || !Number.isFinite(value) || value < 0) return { text: '—' };
  if (value < 1_000) return { text: title };
  if (value < 60_000) return { text: `${formatNumber(value / 1_000, locale, 2)}${locale === 'zh-CN' ? '秒' : ' s'}`, title };
  const seconds = Math.floor(value / 1_000);
  const minutes = Math.floor(seconds / 60);
  const text = minutes < 60
    ? locale === 'zh-CN' ? `${minutes}分${seconds % 60}秒` : `${minutes}m ${seconds % 60}s`
    : locale === 'zh-CN' ? `${Math.floor(minutes / 60)}小时${minutes % 60}分` : `${Math.floor(minutes / 60)}h ${minutes % 60}m`;
  return { text, title };
}

export function analyticsAge(value: number | null | undefined, locale: Locale): string {
  if (value == null || !Number.isFinite(value) || value < 0) return '—';
  const seconds = Math.floor(value / 1_000);
  if (seconds < 1) return locale === 'zh-CN' ? '刚刚' : 'Just now';
  const unit = seconds < 60 ? 'second' : seconds < 3_600 ? 'minute' : seconds < 86_400 ? 'hour' : 'day';
  const amount = Math.floor(seconds / ({ second: 1, minute: 60, hour: 3_600, day: 86_400 }[unit]));
  return new Intl.RelativeTimeFormat(locale === 'en' ? 'en-US' : 'zh-CN', { numeric: 'always', style: 'short' }).format(-amount, unit);
}

export function histogramP95(value: number | null | undefined, capped: boolean | undefined, locale: Locale): FormattedValue {
  if (value == null || !Number.isFinite(value) || value < 0) return { text: '—' };
  const duration = analyticsDuration(value, locale);
  if (capped === true) return { text: locale === 'zh-CN' ? '>1分钟' : '>1 min', title: locale === 'zh-CN' ? 'P95 超过 60 秒，位于当前统计范围之外。' : 'P95 exceeds 60 seconds and is outside the measured range.' };
  if (value >= 60_000 && capped === undefined) return { text: locale === 'zh-CN' ? '最高统计档' : 'Top histogram bucket', title: locale === 'zh-CN' ? '旧版数据将 30–60 秒及以上记录合并在 60,000 ms 统计档。' : 'Legacy data combines 30–60 seconds and above in the 60,000 ms bucket.' };
  return { text: `≤${duration.text}`, title: locale === 'zh-CN' ? `固定直方图 P95 区间上界：${formatMilliseconds(value, locale)}。` : `Fixed-histogram P95 bucket upper bound: ${formatMilliseconds(value, locale)}.` };
}

/** Unknown/capped tails must leave a gap, never a fabricated 60-second plateau. */
export function finiteP95Points(points: UsageAnalysisTimeBucket[]): UsageAnalysisTimeBucket[] {
  return points.map((point) => point.p95_is_capped === true || (point.p95_is_capped === undefined && (point.p95_duration_ms ?? 0) >= 60_000)
    ? { ...point, p95_duration_ms: null } : point);
}

/** Zero-based area, preserving missing-value gaps. No series means no decoration. */
export function metricArea(values: readonly (number | null)[]): string | undefined {
  if (values.length < 2) return undefined;
  const valid = values.filter((value): value is number => value !== null && Number.isFinite(value) && value >= 0);
  if (valid.length < 2) return undefined;
  const maximum = Math.max(...valid);
  if (maximum === 0) return undefined;
  const segments: string[] = []; let run: [number, number][] = [];
  const flush = () => { if (run.length >= 2) segments.push(`M${run[0][0]},48 ${run.map(([x, y]) => `L${x},${y}`).join(' ')} L${run[run.length - 1][0]},48 Z`); run = []; };
  values.forEach((value, index) => {
    if (value === null || !Number.isFinite(value) || value < 0) { flush(); return; }
    run.push([Number((index / (values.length - 1) * 200).toFixed(2)), Number((48 - value / maximum * 42).toFixed(2))]);
  });
  flush(); return segments.join(' ') || undefined;
}
