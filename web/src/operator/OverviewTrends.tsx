import { ChartDataView } from '../charts/ChartDataView';
import { displayTimeZone, bucketTimeZoneNote } from '../charts/displayTimeZone';
import { lazy, Suspense, useMemo } from 'react';
import { api } from '../api';
import { costOption, latencyOption, throughputOption, type UsageChartCopy, type UsageChartFormatters } from '../charts/usageCharts';
import { formatCurrencyDisplay, formatMetricDisplay, formatPercent } from '../format';
import { LocalSettlementNotice, localSettlementLabel, localSettlementTrendLabel } from '../LocalSettlementNotice';
import { useI18n } from '../i18n';
import type { OperatorUsageAnalysisTrends, TypedFilterAst } from '../types';
import { requestDrilldownForOverviewBucket } from './overviewDrilldown';
import { useOperatorResource } from './hooks/useOperatorResource';
import type { ResourceState } from './hooks/useOperatorResource';
import { analyticsDuration, finiteP95Points, histogramP95 } from './analyticsPresentation';
import { statsQuery } from './usageState';
import './overview.css';

const EChart = lazy(() => import('../charts/EChart').then((module) => ({ default: module.EChart })));

function localizedBucketInterval(epoch: number, duration: number, locale: 'en' | 'zh-CN', timeZone: string) {
  const options: Intl.DateTimeFormatOptions = { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit', hour12: false, timeZone };
  return `[${new Date(epoch).toLocaleString(locale, options)}, ${new Date(epoch + duration).toLocaleString(locale, options)})`;
}

function CostLines({ costs, locale }: { costs: OperatorUsageAnalysisTrends['time_series'][number]['costs']; locale: 'zh-CN' | 'en' }) {
  if (!costs.length) return <>—</>;
  return <span className="usage-cost-lines">{[...costs].sort((left, right) => left.currency.localeCompare(right.currency)).map(({ cost, currency }) => <span key={currency} title={formatCurrencyDisplay(cost, currency, locale).title}>{formatCurrencyDisplay(cost, currency, locale).text}</span>)}</span>;
}

/** Independent historical trends: a slow query cannot hide current traffic. */
export function useOverviewTrendResource(token: string, tenant: string) {
  const { t } = useI18n();
  return useOperatorResource(Boolean(token), `${token}\0${tenant}`, () => {
    const query = statsQuery(tenant, {
      preset: '24h', granularity: 'hour', customFrom: '', customTo: '',
      filters: { model: '', keyId: '', upstreamId: '', protocol: '', status: '', errorCode: '' },
    });
    return api<OperatorUsageAnalysisTrends>(`/internal/v1/usage-analysis/trends${query}`, token);
  }, t('usage.loadFailed'));
}

export function OverviewTrends({ state, onDrilldown }: { state: ResourceState<OperatorUsageAnalysisTrends>; onDrilldown?: (ast: TypedFilterAst) => void }) {
  const { locale, t } = useI18n();
  const stats = state.kind === 'ready' ? state.value : undefined;
  const copy: UsageChartCopy = useMemo(() => ({
    requests: t('usage.requests'), success: t('traffic.success'), failures: t('traffic.failure'),
    averageLatency: t('usage.average'), p95Latency: t('usage.p95Approx'),
    cost: localSettlementLabel(locale), noData: t('usage.noData'),
  }), [locale, t]);
  const format: UsageChartFormatters = useMemo(() => {
    const timeZone = displayTimeZone();
    const duration = stats?.granularity === 'day' ? 86_400_000 : 3_600_000;
    const options: Intl.DateTimeFormatOptions = { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit', hour12: false, timeZone };
    return {
      bucket: (value) => new Date(value).toLocaleString(locale, options),
      bucketInterval: (value) => localizedBucketInterval(value, duration, locale, timeZone),
      cost: (value, currency) => formatCurrencyDisplay(value, currency, locale).text,
      duration: (value) => analyticsDuration(value, locale).text,
      number: (value) => formatMetricDisplay(value, locale).text,
      percent: (value) => formatPercent(value, locale),
    };
  }, [locale, stats?.granularity]);
  const throughput = useMemo(() => throughputOption(stats?.time_series ?? [], copy, format), [stats, copy, format]);
  const latency = useMemo(() => latencyOption(finiteP95Points(stats?.time_series ?? []), copy, format), [stats, copy, format]);
  const costs = useMemo(() => costOption(stats?.time_series ?? [], copy, format), [stats, copy, format]);
  const trendCards = [
    { id: 'throughput', title: t('usage.throughput'), option: throughput },
    { id: 'latency', title: t('usage.latencyTrend'), option: latency },
    { id: 'cost', title: localSettlementTrendLabel(locale), option: costs },
  ];
  const drillDown = (bucketStart: number) => {
    if (!stats || !onDrilldown) return;
    const ast = requestDrilldownForOverviewBucket(bucketStart, stats.granularity);
    if (ast) onDrilldown(ast);
  };

  const table = stats && <div className="overview-trend-data" aria-label={t('usage.trendData')}>
        <div className="table-scroll"><table>
          <thead><tr><th>{t('request.time')} · {displayTimeZone()}</th><th>{copy.success}</th><th>{copy.failures}</th><th>{copy.averageLatency}</th><th>{copy.p95Latency}</th><th><LocalSettlementNotice /></th></tr></thead>
          <tbody>{stats.time_series.map((point) => <tr key={point.bucket_start}>
            <td>{onDrilldown ? <button type="button" className="table-link overview-trend-bucket" onClick={() => drillDown(point.bucket_start)}>{format.bucket(point.bucket_start)}</button> : format.bucket(point.bucket_start)}</td><td>{format.number(point.success)}</td><td>{format.number(point.failed)}</td>
            <td title={analyticsDuration(point.avg_duration_ms, locale).title}>{analyticsDuration(point.avg_duration_ms, locale).text}</td><td title={histogramP95(point.p95_duration_ms, point.p95_is_capped, locale).title}>{histogramP95(point.p95_duration_ms, point.p95_is_capped, locale).text}</td><td><CostLines costs={point.costs} locale={locale} /></td>
          </tr>)}</tbody>
        </table></div>
      </div>;

  return <section className="overview-trends" aria-label={t('usage.trend')}>
    {state.kind === 'failed' && <div className="notice error" role="alert">{state.message}</div>}
    {state.kind === 'ready' && state.refreshError && <div className="notice error" role="alert">{state.refreshError}</div>}
    {!stats && state.kind !== 'failed' && <div className="panel empty" role="status">{t('common.loading')}</div>}
    {stats && <>
      <p className="usage-time-zone">{bucketTimeZoneNote(locale, stats.time_zone)}</p>
      <p className="analytics-settlement-note"><LocalSettlementNotice /></p>
      <p className="analytics-p95-note">{locale === 'zh-CN' ? 'P95 按直方图区间上界展示；超出最高档及早期无法确定的值留空。' : 'P95 uses histogram upper bounds. Overflow and ambiguous earlier values leave gaps.'}</p>
      <div className="overview-trend-grid">
        {trendCards.map(({ id, title, option }) => <article className={`panel overview-trend-card${id === 'throughput' ? ' overview-trend-primary' : ''}`} key={id}>
          <ChartDataView title={title} metadata={<span>{displayTimeZone()}</span>} data={table}>
          {stats.time_series.length === 0 ? <div className="empty">{t('usage.noData')}</div>
            : <Suspense fallback={<div className="empty">{t('common.loading')}</div>}>
              <EChart ariaLabel={title} locale={locale} option={option} timeZone={displayTimeZone()} onClick={({ dataIndex }) => {
                const point = stats.time_series[dataIndex];
                if (point) drillDown(point.bucket_start);
              }} />
            </Suspense>}
          </ChartDataView>
        </article>)}
      </div>

    </>}
  </section>;
}
