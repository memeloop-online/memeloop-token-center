import { LocalSettlementNotice, localSettlementLabel } from '../LocalSettlementNotice';
import { displayTimeZone } from '../charts/displayTimeZone';
import { totalTokens } from '../charts/usageCharts';
import { formatCurrencyDisplay, formatMetricDisplay, formatNumber, formatPercent } from '../format';
import { useI18n } from '../i18n';
import { DetailTooltip } from '../design-system';
import type { OperatorUsageAnalysis, UsageAnalysisMetrics } from '../types';
import { AnalyticsMetric } from './AnalyticsMetric';
import {
  analyticsDuration,
  averageBucketTpsSeries,
  averageSeriesTps,
  finiteP95Points,
  formatTps,
  histogramP95,
  seriesTpsSamples,
} from './analyticsPresentation';

export type UsageSummaryStats = Pick<OperatorUsageAnalysis, 'summary' | 'time_series'>;

export function UsageSummaryMetrics({ stats, currency, timeZone = displayTimeZone() }: {
  stats: UsageSummaryStats;
  /** Keep a portal credential's billing currency scoped to its own cards. */
  currency?: string;
  /** Bucket labels stay absolute; this only controls interactive display text. */
  timeZone?: string;
}) {
  const { locale, t } = useI18n();
  const { summary, time_series: points } = stats;
  const rate = summary.requests > 0 ? summary.success / summary.requests : null;
  // UsageAnalysis input_tokens excludes the cached portion; the denominator is
  // everything that could have been read or written through the cache.
  const cacheRateOf = (metrics: Pick<UsageAnalysisMetrics, 'input_tokens' | 'cached_input_tokens' | 'cache_write_tokens'>) => {
    const denominator = metrics.input_tokens + metrics.cached_input_tokens + metrics.cache_write_tokens;
    if (!Number.isFinite(metrics.cached_input_tokens) || !Number.isFinite(denominator) || denominator <= 0) return null;
    return metrics.cached_input_tokens / denominator;
  };
  const cacheRate = cacheRateOf(summary);
  const number = (value: number) => formatMetricDisplay(value, locale);
  const exactValue = (text: string, title?: string) => <span className="metric-number"><span className="metric-exact" title={title ?? text}>{text}</span></span>;
  const numeric = (label: string, value: number, trend: number[], tone = '') => <AnalyticsMetric timestamps={points.map(point => point.bucket_start)} timeZone={timeZone} label={label} value={exactValue(number(value).text, number(value).title)} trend={trend} tone={tone} />;
  const summarySamples = summary.output_rate && Number.isFinite(summary.output_rate.requests) && summary.output_rate.requests >= 0 ? summary.output_rate.requests : seriesTpsSamples(points);
  const tpsHint = <>{t('usage.averageTpsHint')} {summarySamples === null ? t('usage.tpsEligibleSamplesUnknown') : t('usage.tpsEligibleSamples', { count: formatNumber(summarySamples, locale, 0) })}</>;
  const tpsMetric = (label: string, value: ReturnType<typeof formatTps>, trend: Array<number | null>) => <AnalyticsMetric timestamps={points.map(point => point.bucket_start)} timeZone={timeZone} label={label} labelContent={<DetailTooltip content={tpsHint}><span tabIndex={0}>{label}</span></DetailTooltip>} value={exactValue(value.text, value.title)} title={value.title} formatSample={(sample) => formatTps(sample, locale).title ?? formatTps(sample, locale).text} trend={trend} />;
  const average = analyticsDuration(summary.avg_duration_ms, locale);
  const p95 = histogramP95(summary.p95_duration_ms, summary.p95_is_capped, locale);
  const averageTpsTrend = averageBucketTpsSeries(points);
  const averageTps = formatTps(averageSeriesTps(points), locale);
  const selectedCosts = currency ? summary.costs.filter((cost) => cost.currency === currency) : summary.costs;
  const trendCurrency = currency ?? (summary.costs.length === 1 ? summary.costs[0].currency : undefined);
  return <section className="metrics usage-metrics" aria-label={t('usage.tab.overview')}>
    {numeric(t('usage.totalTokens'), totalTokens(summary), points.map(totalTokens))}
    <AnalyticsMetric timestamps={points.map(point => point.bucket_start)} timeZone={timeZone} label={t('usage.cacheRate')} labelContent={<DetailTooltip content={t('usage.cacheRateHint')}><span tabIndex={0}>{t('usage.cacheRate')}</span></DetailTooltip>} value={formatPercent(cacheRate, locale)} ratio={cacheRate} formatSample={(value) => formatPercent(value, locale)} trend={points.map(cacheRateOf)} />
    <AnalyticsMetric timestamps={points.map(point => point.bucket_start)} timeZone={timeZone} label={t('usage.successRate')} value={formatPercent(rate, locale)} ratio={rate} />
    <AnalyticsMetric timestamps={points.map(point => point.bucket_start)} timeZone={timeZone} label={localSettlementLabel(locale)} labelContent={<LocalSettlementNotice />} value={selectedCosts.length ? <span className="usage-cost-lines">{selectedCosts.map(({ cost, currency: costCurrency }) => { const display = formatCurrencyDisplay(cost, costCurrency, locale); return <span key={costCurrency} title={display.title}>{display.text}</span>; })}</span> : '—'} formatSample={(_value, index) => { const cost = trendCurrency ? points[index].costs.find(item => item.currency === trendCurrency) : undefined; return cost ? formatCurrencyDisplay(cost.cost, cost.currency, locale).title ?? '—' : '—'; }} trend={trendCurrency ? points.map((point) => point.costs.some(cost => cost.currency === trendCurrency) ? Number(point.costs.find(cost => cost.currency === trendCurrency)!.cost) : null) : undefined} />
    {numeric(t('usage.generationUnits'), summary.generation_units, points.map((point) => point.generation_units))}
    {numeric(t('usage.cachedTokens'), summary.cached_input_tokens, points.map((point) => point.cached_input_tokens))}
    {numeric(t('usage.cacheWriteTokens'), summary.cache_write_tokens, points.map((point) => point.cache_write_tokens))}
    {tpsMetric(t('usage.averageTps'), averageTps, averageTpsTrend)}
    <AnalyticsMetric timestamps={points.map(point => point.bucket_start)} timeZone={timeZone} label={t('usage.average')} value={average.text} title={average.title} formatSample={value => analyticsDuration(value, locale).title ?? analyticsDuration(value, locale).text} trend={points.map((point) => point.avg_duration_ms)} />
    <AnalyticsMetric timestamps={points.map(point => point.bucket_start)} timeZone={timeZone} label={t('usage.p95Approx')} value={p95.text} title={p95.title} formatSample={(_value, index) => { const point = points[index]; const display = histogramP95(point.p95_duration_ms, point.p95_is_capped, locale); return `${display.text} ${display.title ?? ''}`; }} trend={finiteP95Points(points).map((point) => point.p95_duration_ms)} />
  </section>;
}
