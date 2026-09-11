import { lazy, Suspense, useMemo } from 'react';
import { api } from '../api';
import { costOption, latencyOption, throughputOption, type UsageChartCopy, type UsageChartFormatters } from '../charts/usageCharts';
import { formatCurrency, formatMilliseconds, formatNumber, formatPercent } from '../format';
import { useI18n } from '../i18n';
import type { OperatorUsageAnalysisTrends, TypedFilterAst } from '../types';
import { requestDrilldownForOverviewBucket } from './overviewDrilldown';
import { useOperatorResource } from './hooks/useOperatorResource';
import { statsQuery } from './usageState';
import './overview.css';

const EChart = lazy(() => import('../charts/EChart').then((module) => ({ default: module.EChart })));

function formatCosts(costs: OperatorUsageAnalysisTrends['time_series'][number]['costs'], locale: 'zh-CN' | 'en') {
  if (!costs.length) return '—';
  return [...costs]
    .sort((left, right) => left.currency.localeCompare(right.currency))
    .map(({ cost, currency }) => formatCurrency(cost, currency, locale))
    .join(' · ');
}

/** Independent historical trends: a slow query cannot hide current traffic. */
export function OverviewTrends({ token, tenant, onDrilldown }: { token: string; tenant: string; onDrilldown?: (ast: TypedFilterAst) => void }) {
  const { locale, t } = useI18n();
  const resource = useOperatorResource(Boolean(token), `${token}\0${tenant}`, () => {
    const query = statsQuery(tenant, {
      preset: '24h', granularity: 'hour', customFrom: '', customTo: '',
      filters: { model: '', keyId: '', upstreamId: '', protocol: '', status: '', errorCode: '' },
    });
    return api<OperatorUsageAnalysisTrends>(`/internal/v1/usage-analysis/trends${query}`, token);
  }, t('usage.loadFailed'));
  const stats = resource.state.kind === 'ready' ? resource.state.value : undefined;
  const copy: UsageChartCopy = useMemo(() => ({
    requests: t('usage.requests'), success: t('traffic.success'), failures: t('traffic.failure'),
    averageLatency: t('usage.average'), p95Latency: t('usage.p95Approx'),
    cost: t('traffic.cost'), noData: t('usage.noData'),
  }), [t]);
  const format: UsageChartFormatters = useMemo(() => ({
    bucket: (value) => new Date(value).toLocaleString(locale, {
      month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit', hour12: false, timeZone: 'UTC',
    }),
    cost: (value, currency) => formatCurrency(value, currency, locale),
    duration: (value) => formatMilliseconds(value, locale),
    number: (value) => formatNumber(value, locale),
    percent: (value) => formatPercent(value, locale),
  }), [locale]);
  const throughput = useMemo(() => throughputOption(stats?.time_series ?? [], copy, format), [stats, copy, format]);
  const latency = useMemo(() => latencyOption(stats?.time_series ?? [], copy, format), [stats, copy, format]);
  const costs = useMemo(() => costOption(stats?.time_series ?? [], copy, format), [stats, copy, format]);
  const trendCards = [
    { id: 'throughput', title: t('usage.throughput'), option: throughput },
    { id: 'latency', title: t('usage.latencyTrend'), option: latency },
    { id: 'cost', title: t('usage.costTrend'), option: costs },
  ];
  const drillDown = (bucketStart: number) => {
    if (!stats || !onDrilldown) return;
    const ast = requestDrilldownForOverviewBucket(bucketStart, stats.granularity);
    if (ast) onDrilldown(ast);
  };

  return <section className="overview-trends" aria-label={t('usage.trend')}>
    {resource.state.kind === 'failed' && <div className="notice error" role="alert">{resource.state.message}</div>}
    {resource.state.kind === 'ready' && resource.state.refreshError && <div className="notice error" role="alert">{resource.state.refreshError}</div>}
    {!stats && resource.state.kind !== 'failed' && <div className="panel empty" role="status">{t('common.loading')}</div>}
    {stats && <>
      <div className="overview-trend-grid">
        {trendCards.map(({ id, title, option }) => <article className={`panel overview-trend-card${id === 'throughput' ? ' overview-trend-primary' : ''}`} key={id}>
          <div className="panel-title"><h2>{title}</h2><span>{stats.time_zone}</span></div>
          {stats.time_series.length === 0 ? <div className="empty">{t('usage.noData')}</div>
            : <Suspense fallback={<div className="empty">{t('common.loading')}</div>}>
              <EChart ariaLabel={title} locale={locale} option={option} timeZone={stats.time_zone} onClick={({ dataIndex }) => {
                const point = stats.time_series[dataIndex];
                if (point) drillDown(point.bucket_start);
              }} />
            </Suspense>}
        </article>)}
      </div>
      <details className="overview-trend-data">
        <summary>{t('usage.trendData')}</summary>
        <div className="table-scroll"><table>
          <thead><tr><th>{t('request.time')} · {stats.time_zone}</th><th>{copy.success}</th><th>{copy.failures}</th><th>{copy.averageLatency}</th><th>{copy.p95Latency}</th><th>{copy.cost}</th></tr></thead>
          <tbody>{stats.time_series.map((point) => <tr key={point.bucket_start}>
            <td>{onDrilldown ? <button type="button" className="table-link overview-trend-bucket" onClick={() => drillDown(point.bucket_start)}>{format.bucket(point.bucket_start)}</button> : format.bucket(point.bucket_start)}</td><td>{format.number(point.success)}</td><td>{format.number(point.failed)}</td>
            <td>{formatMilliseconds(point.avg_duration_ms, locale)}</td><td>{formatMilliseconds(point.p95_duration_ms, locale)}</td><td>{formatCosts(point.costs, locale)}</td>
          </tr>)}</tbody>
        </table></div>
      </details>
    </>}
  </section>;
}
