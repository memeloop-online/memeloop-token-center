import type { ReactNode } from 'react';
import { LocalSettlementNotice, localSettlementLabel } from '../LocalSettlementNotice';
import { displayTimeZone } from '../charts/displayTimeZone';
import { formatCurrencyDisplay, formatMetricDisplay, formatNumber, formatPercent } from '../format';
import { useI18n } from '../i18n';
import { DetailTooltip } from '../design-system';
import type { MonitoringHealth, OperatorMonitoringSnapshot, UsageAnalysisCost, UsageAnalysisTimeBucket } from '../types';
import { monitoringModelGroups } from './monitoringAccountGroups';
import { AnalyticsMetric } from './AnalyticsMetric';
import { analyticsAge, analyticsDuration, averageBucketTpsSeries, averageSeriesTps, formatTps, histogramP95, seriesTpsSamples } from './analyticsPresentation';

function CostLines({ costs }: { costs: UsageAnalysisCost[] }) {
  const { locale } = useI18n();
  if (!costs.length) return <>—</>;
  return <span className="monitoring-cost-lines">{costs.map(({ currency, cost }) => (
    <span key={currency} title={formatCurrencyDisplay(cost, currency, locale).title}>{formatCurrencyDisplay(cost, currency, locale).text}</span>
  ))}</span>;
}

export function healthClass(status: MonitoringHealth['status']) {
  if (status === 'healthy') return 'ok';
  if (status === 'unhealthy') return 'bad';
  if (status === 'degraded') return 'pending';
  return 'unknown';
}

export function RoutingStatusBadge({ health }: { health: MonitoringHealth }) {
  const { t } = useI18n();
  return <span className="monitoring-routing-status" title={health.version}><small>{t('monitoring.routingStatus')}</small><span className={`status ${healthClass(health.status)}`}>{t(`monitoring.health.${health.status}`)}</span></span>;
}

function Freshness({ snapshot }: { snapshot: OperatorMonitoringSnapshot }) {
  const { locale, t } = useI18n();
  const freshness = snapshot.freshness;
  if (freshness.latest_terminal_created_at === null) return <span>{t('monitoring.noTerminalTraffic')}</span>;
  const occurred = new Date(freshness.latest_terminal_created_at).toLocaleString(locale);
  return <span title={occurred}>{analyticsAge(freshness.age_millis, locale)}</span>;
}

function MonitoringMetricList({ metrics }: { metrics: OperatorMonitoringSnapshot['summary'] }) {
  const { locale, t } = useI18n();
  const terminal = metrics.successful_requests + metrics.failed_requests;
  const successRate = terminal > 0 ? metrics.successful_requests / terminal : null;
  return <dl className="monitoring-metric-list">
    <div><dt>{t('usage.requests')}</dt><dd title={formatMetricDisplay(metrics.requests, locale).title}>{formatMetricDisplay(metrics.requests, locale).text}</dd></div>
    <div><dt>{t('usage.successRate')}</dt><dd>{formatPercent(successRate, locale)}</dd></div>
    <div><dt>{t('usage.average')}</dt><dd title={analyticsDuration(metrics.avg_duration_ms, locale).title}>{analyticsDuration(metrics.avg_duration_ms, locale).text}</dd></div>
    <div><dt>{t('usage.p95Approx')}</dt><dd title={histogramP95(metrics.p95_duration_ms, metrics.p95_is_capped, locale).title}>{histogramP95(metrics.p95_duration_ms, metrics.p95_is_capped, locale).text}</dd></div>
    <div><dt>{t('traffic.cost')}</dt><dd><CostLines costs={metrics.costs} /></dd></div>
  </dl>;
}

export function MonitoringSnapshot({ snapshot, points = [], quotaSummary }: { snapshot: OperatorMonitoringSnapshot; points?: UsageAnalysisTimeBucket[]; quotaSummary?: ReactNode }) {
  const { locale, t } = useI18n();
  const summary = snapshot.summary;
  const successRate = summary.requests > 0 ? summary.successful_requests / summary.requests : null;
  const range = `${new Date(snapshot.from_created_at).toLocaleString(locale)} – ${new Date(snapshot.to_created_at).toLocaleString(locale)}`;
  const count = (value: number) => formatMetricDisplay(value, locale);
  const totalTokens: { text: string; title?: string } = summary.total_tokens === undefined ? { text: '—' } : formatMetricDisplay(summary.total_tokens, locale);
  const cacheRate = summary.cache_rate ?? null;
  const average = analyticsDuration(summary.avg_duration_ms, locale);
  const averageTpsTrend = averageBucketTpsSeries(points);
  const averageTps = formatTps(averageSeriesTps(points), locale);
  const tpsSamples = seriesTpsSamples(points);
  const tpsHint = <>{t('usage.averageTpsHint')} {tpsSamples === null ? t('usage.tpsEligibleSamplesUnknown') : t('usage.tpsEligibleSamples', { count: formatNumber(tpsSamples, locale, 0) })}</>;
  const currency = summary.costs.length === 1 ? summary.costs[0].currency : undefined;
  return <section className="operator-monitoring" aria-labelledby="monitoring-heading">
    <article className="panel">
      <div className="panel-title monitoring-heading">
        <div><h2 id="monitoring-heading">{t('monitoring.title')}</h2><p className="muted">{range}</p></div>
        <RoutingStatusBadge health={snapshot.health} />
      </div>
      <section className="metrics operator-monitoring-metrics monitoring-metrics-grid" aria-label={t('monitoring.summary')}>
        <AnalyticsMetric label={t('usage.totalTokens')} value={totalTokens.text} title={totalTokens.title} />
        <AnalyticsMetric timestamps={points.map(point => point.bucket_start)} timeZone={displayTimeZone()} label={t('traffic.success')} value={count(summary.successful_requests).text} title={count(summary.successful_requests).title} tone="positive" trend={points.map((point) => point.success)} ratio={successRate} />
        <AnalyticsMetric label={t('usage.cacheRate')} value={formatPercent(cacheRate, locale)} ratio={cacheRate} />
        <AnalyticsMetric timestamps={points.map(point => point.bucket_start)} timeZone={displayTimeZone()} label={t('usage.successRate')} value={formatPercent(successRate, locale)} ratio={successRate} />
        <AnalyticsMetric timestamps={points.map(point => point.bucket_start)} timeZone={displayTimeZone()} label={t('usage.averageTps')} labelContent={<DetailTooltip content={tpsHint}><span tabIndex={0}>{t('usage.averageTps')}</span></DetailTooltip>} value={averageTps.text} title={averageTps.title} formatSample={(value) => formatTps(value, locale).title ?? formatTps(value, locale).text} trend={averageTpsTrend} />
        <AnalyticsMetric timestamps={points.map(point => point.bucket_start)} timeZone={displayTimeZone()} label={t('usage.average')} value={average.text} title={average.title} formatSample={value => analyticsDuration(value, locale).title ?? analyticsDuration(value, locale).text} trend={points.map((point) => point.avg_duration_ms)} />
        <AnalyticsMetric timestamps={points.map(point => point.bucket_start)} timeZone={displayTimeZone()} label={localSettlementLabel(locale)} labelContent={<LocalSettlementNotice />} value={<CostLines costs={summary.costs} />} formatSample={(_value, index) => { const cost = points[index].costs.find(item => item.currency === currency); return cost ? formatCurrencyDisplay(cost.cost, cost.currency, locale).title ?? '—' : '—'; }} trend={currency ? points.map((point) => point.costs.some(cost => cost.currency === currency) ? Number(point.costs.find(cost => cost.currency === currency)!.cost) : null) : undefined} />
        <AnalyticsMetric timestamps={points.map(point => point.bucket_start)} timeZone={displayTimeZone()} label={t('monitoring.freshness')} value={<Freshness snapshot={snapshot} />} />
      </section>
    </article>
    {quotaSummary}
    <article className="panel monitoring-top-panel">
      <div className="panel-title"><h2>{t('monitoring.topUpstreams')}</h2><span>{t('monitoring.topAccountModelsScope')}</span></div>
      {!snapshot.top_upstream_models.length
        ? <div className="empty">{t('monitoring.noStableUpstreamTraffic')}</div>
        : <ul className="monitoring-top-list">{monitoringModelGroups(snapshot.top_upstream_models).map((group) => <li key={group.model} data-upstream-model={group.model}>
          <div className="monitoring-account-heading"><code>{group.model}</code></div>
          <ul className="monitoring-account-models">{group.accounts.map((value, index) => {
          const metrics = value.metrics;
          return <li key={`${value.upstream_account_id}\0${index}`} data-upstream-account-id={value.upstream_account_id}>
            <div className="monitoring-top-heading">
              <div><b title={`${locale === 'zh-CN' ? '账号 ID' : 'Account ID'}: ${value.upstream_account_id}`}>{value.upstream_name === value.upstream_account_id ? (locale === 'zh-CN' ? '未命名账号' : 'Unnamed account') : value.upstream_name}</b></div>
              <RoutingStatusBadge health={value.health} />
            </div>
            <MonitoringMetricList metrics={metrics} />
            <ol className="monitoring-outcomes" aria-label={t('monitoring.terminalOutcomes')}>
              {value.terminal_outcomes.slice(0, 5).map((outcome) => <li key={`${outcome.source}\0${outcome.id}`}>
                <time dateTime={new Date(outcome.created_at).toISOString()} title={new Date(outcome.created_at).toLocaleString(locale)}>{analyticsAge(Math.max(0, snapshot.generated_at - outcome.created_at), locale)}</time>
                <span className={`status ${outcome.status === 'success' ? 'ok' : 'bad'}`}>{t(`monitoring.outcome.${outcome.status}`)}</span>
                <span>{t(`monitoring.source.${outcome.source}`)}</span>
                <span title={analyticsDuration(outcome.duration_ms, locale).title}>{analyticsDuration(outcome.duration_ms, locale).text}</span>
                {outcome.error_code && <code>{outcome.error_code}</code>}
              </li>)}
            </ol>
          </li>;
          })}</ul>
        </li>)}</ul>}
    </article>
  </section>;
}
