import { formatMetricDisplay, formatMilliseconds, formatNumber, type FormattedValue } from '../format.js';
import type { Locale } from '../i18n.js';
import type { UsageAnalysisTimeBucket } from '../types.js';

export function averageBucketTps(point: Pick<UsageAnalysisTimeBucket, 'avg_duration_ms' | 'output_tokens' | 'requests'>): number | null {
  if (!Number.isFinite(point.output_tokens) || point.output_tokens < 0) return null;
  if (!Number.isFinite(point.requests) || point.requests <= 0) return null;
  if (point.avg_duration_ms == null || !Number.isFinite(point.avg_duration_ms) || point.avg_duration_ms <= 0) return null;
  return point.output_tokens / (point.avg_duration_ms * point.requests / 1_000);
}

export function averageBucketTpsSeries(points: readonly Pick<UsageAnalysisTimeBucket, 'avg_duration_ms' | 'output_tokens' | 'requests'>[]): Array<number | null> {
  return points.map((point) => averageBucketTps(point));
}

// Usage facts and their rollups increment requests and duration_count together
// (including failed requests and generation jobs). avg_duration_ms therefore
// reconstructs total recorded duration, not a mean of per-request TPS values.
// The API has no per-request TPS distribution: bucket averages cannot supply P95.
export function averageSeriesTps(points: readonly Pick<UsageAnalysisTimeBucket, 'avg_duration_ms' | 'output_tokens' | 'requests'>[]): number | null {
  let outputTokens = 0;
  let durationMillis = 0;
  for (const point of points) {
    if (!Number.isFinite(point.output_tokens) || point.output_tokens < 0) continue;
    if (!Number.isFinite(point.requests) || point.requests <= 0) continue;
    if (point.avg_duration_ms == null || !Number.isFinite(point.avg_duration_ms) || point.avg_duration_ms <= 0) continue;
    outputTokens += point.output_tokens;
    durationMillis += point.avg_duration_ms * point.requests;
  }
  if (durationMillis <= 0) return null;
  return outputTokens / (durationMillis / 1_000);
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
  if (capped === true) return { text: locale === 'zh-CN' ? '>1分钟' : '>1 min', title: locale === 'zh-CN' ? 'P95超过60秒；精确值未知，超过当前统计范围。' : 'P95 exceeds 60 seconds. Its exact value is unknown and outside the measured range.' };
  if (value >= 60_000 && capped === undefined) return { text: locale === 'zh-CN' ? '最高统计档' : 'Top histogram bucket', title: locale === 'zh-CN' ? '旧版数据将30–60秒和超过60秒的区间都记为60,000ms，无法区分是否截顶；不是实际P95。' : 'Legacy data maps both 30–60 seconds and >60 seconds to 60,000ms. The exact P95 and overflow state are unknown.' };
  return { text: `≤${duration.text}`, title: locale === 'zh-CN' ? `固定直方图P95所在区间的上界（${formatMilliseconds(value, locale)}），不是精确分位数。` : `Upper bound of the fixed-histogram P95 bucket (${formatMilliseconds(value, locale)}), not an exact percentile.` };
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
