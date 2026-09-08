import { lazy, Suspense, useMemo } from 'react';
import { api } from '../api';
import { latencyOption, throughputOption, type UsageChartCopy, type UsageChartFormatters } from '../charts/usageCharts';
import { formatCurrency, formatMilliseconds, formatNumber, formatPercent } from '../format';
import { useI18n } from '../i18n';
import type { OperatorUsageAnalysis } from '../types';
import { useOperatorResource } from './hooks/useOperatorResource';
import { statsQuery } from './usageState';
import './overview.css';

const EChart = lazy(() => import('../charts/EChart').then((module) => ({ default: module.EChart })));

/** Independent historical trends: a slow query cannot hide current traffic. */
export function OverviewTrends({ token, tenant }: { token: string; tenant: string }) {
  const { locale, t } = useI18n();
  const resource = useOperatorResource(Boolean(token), `${token}\0${tenant}`, () => {
    const query = statsQuery(tenant, {
      preset: '24h', granularity: 'hour', customFrom: '', customTo: '',
      filters: { model: '', keyId: '', upstreamId: '', protocol: '', status: '', errorCode: '' },
    });
    return api<OperatorUsageAnalysis>(`/internal/v1/usage-analysis${query}`, token);
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

  return <section className="overview-trends" aria-label={t('usage.trend')}>
    {resource.state.kind === 'failed' && <div className="notice error" role="alert">{resource.state.message}</div>}
    {resource.state.kind === 'ready' && resource.state.refreshError && <div className="notice error" role="alert">{resource.state.refreshError}</div>}
    {!stats && resource.state.kind !== 'failed' && <div className="panel empty" role="status">{t('common.loading')}</div>}
    {stats && <>
      <div className="overview-trend-grid">
        {[
          { title: t('usage.throughput'), option: throughput },
          { title: t('usage.latencyTrend'), option: latency },
        ].map(({ title, option }) => <article className="panel overview-trend-card" key={title}>
          <div className="panel-title"><h2>{title}</h2><span>{stats.time_zone}</span></div>
          {stats.time_series.length === 0 ? <div className="empty">{t('usage.noData')}</div>
            : <Suspense fallback={<div className="empty">{t('common.loading')}</div>}>
              <EChart ariaLabel={title} locale={locale} option={option} timeZone={stats.time_zone} />
            </Suspense>}
        </article>)}
      </div>
      <details className="overview-trend-data">
        <summary>{t('usage.trendData')}</summary>
        <div className="table-scroll"><table>
          <thead><tr><th>{t('request.time')} · UTC</th><th>{copy.success}</th><th>{copy.failures}</th><th>{copy.averageLatency}</th><th>{copy.p95Latency}</th></tr></thead>
          <tbody>{stats.time_series.map((point) => <tr key={point.bucket_start}>
            <td>{format.bucket(point.bucket_start)}</td><td>{format.number(point.success)}</td><td>{format.number(point.failed)}</td>
            <td>{formatMilliseconds(point.avg_duration_ms, locale)}</td><td>{formatMilliseconds(point.p95_duration_ms, locale)}</td>
          </tr>)}</tbody>
        </table></div>
      </details>
    </>}
  </section>;
}
