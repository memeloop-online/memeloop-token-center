import { LocalSettlementNotice, localSettlementLabel } from '../LocalSettlementNotice';
import { displayTimeZone } from '../charts/displayTimeZone';
import { totalTokens } from '../charts/usageCharts';
import { formatCurrencyDisplay, formatMetricDisplay, formatPercent } from '../format';
import { useI18n } from '../i18n';
import { DetailTooltip } from '../design-system';
import type { OperatorUsageAnalysis } from '../types';
import { AnalyticsMetric } from './AnalyticsMetric';
import {
  analyticsDuration,
  averageBucketTpsSeries,
  averageSeriesTps,
  finiteP95Points,
  formatTps,
  histogramP95,
} from './analyticsPresentation';

export function UsageSummaryMetrics({ stats }: { stats: OperatorUsageAnalysis }) {
  const { locale, t } = useI18n();
  const { summary, time_series: points } = stats;
  const rate = summary.requests > 0 ? summary.success / summary.requests : null;
  const number = (value: number) => formatMetricDisplay(value, locale);
  const exactValue = (text: string, title?: string) => <span className="metric-number"><span className="metric-exact" title={title ?? text}>{text}</span></span>;
  const numeric = (label: string, value: number, trend: number[], tone = '') => <AnalyticsMetric timestamps={points.map(point => point.bucket_start)} timeZone={displayTimeZone()} label={label} value={exactValue(number(value).text, number(value).title)} trend={trend} tone={tone} />;
  const tpsMetric = (label: string, value: ReturnType<typeof formatTps>, trend: Array<number | null>) => <AnalyticsMetric timestamps={points.map(point => point.bucket_start)} timeZone={displayTimeZone()} label={label} labelContent={<DetailTooltip content={t('usage.averageTpsHint')}><span tabIndex={0}>{label}</span></DetailTooltip>} value={exactValue(value.text, value.title)} title={value.title} formatSample={(sample) => formatTps(sample, locale).title ?? formatTps(sample, locale).text} trend={trend} />;
  const average = analyticsDuration(summary.avg_duration_ms, locale);
  const p95 = histogramP95(summary.p95_duration_ms, summary.p95_is_capped, locale);
  const averageTpsTrend = averageBucketTpsSeries(points);
  const averageTps = formatTps(averageSeriesTps(points), locale);
  const currency = summary.costs.length === 1 ? summary.costs[0].currency : undefined;
  return <section className="metrics usage-metrics" aria-label={t('usage.tab.overview')}>
    {numeric(t('usage.requests'), summary.requests, points.map((point) => point.requests))}
    <AnalyticsMetric timestamps={points.map(point => point.bucket_start)} timeZone={displayTimeZone()} label={t('usage.successRate')} value={formatPercent(rate, locale)} ratio={rate} />
    {numeric(t('usage.failures'), summary.failed, points.map((point) => point.failed), 'negative')}
    <AnalyticsMetric timestamps={points.map(point => point.bucket_start)} timeZone={displayTimeZone()} label={localSettlementLabel(locale)} labelContent={<LocalSettlementNotice />} value={summary.costs.length ? <span className="usage-cost-lines">{summary.costs.map(({ cost, currency }) => { const display = formatCurrencyDisplay(cost, currency, locale); return <span key={currency} title={display.title}>{display.text}</span>; })}</span> : '—'} formatSample={(_value, index) => { const cost = points[index].costs.find(item => item.currency === currency); return cost ? formatCurrencyDisplay(cost.cost, cost.currency, locale).title ?? '—' : '—'; }} trend={currency ? points.map((point) => point.costs.some(cost => cost.currency === currency) ? Number(point.costs.find(cost => cost.currency === currency)!.cost) : null) : undefined} />
    {numeric(t('usage.totalTokens'), totalTokens(summary), points.map(totalTokens))}
    {numeric(t('usage.generationUnits'), summary.generation_units, points.map((point) => point.generation_units))}
    {numeric(t('usage.cachedTokens'), summary.cached_input_tokens, points.map((point) => point.cached_input_tokens))}
    {numeric(t('usage.cacheWriteTokens'), summary.cache_write_tokens, points.map((point) => point.cache_write_tokens))}
    {tpsMetric(t('usage.averageTps'), averageTps, averageTpsTrend)}
    <AnalyticsMetric timestamps={points.map(point => point.bucket_start)} timeZone={displayTimeZone()} label={t('usage.average')} value={average.text} title={average.title} formatSample={value => analyticsDuration(value, locale).title ?? analyticsDuration(value, locale).text} trend={points.map((point) => point.avg_duration_ms)} />
    <AnalyticsMetric timestamps={points.map(point => point.bucket_start)} timeZone={displayTimeZone()} label={t('usage.p95Approx')} value={p95.text} title={p95.title} formatSample={(_value, index) => { const point = points[index]; const display = histogramP95(point.p95_duration_ms, point.p95_is_capped, locale); return `${display.text} ${display.title ?? ''}`; }} trend={finiteP95Points(points).map((point) => point.p95_duration_ms)} />
  </section>;
}
