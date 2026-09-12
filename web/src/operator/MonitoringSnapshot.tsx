import { Metric, NumberMetric } from '../components';
import { formatCurrency, formatElapsedTime, formatMilliseconds, formatNumber, formatPercent } from '../format';
import { useI18n } from '../i18n';
import type { MonitoringHealth, OperatorMonitoringSnapshot, UsageAnalysisCost } from '../types';
import { monitoringModelGroups } from './monitoringAccountGroups';

function CostLines({ costs }: { costs: UsageAnalysisCost[] }) {
  const { locale } = useI18n();
  if (!costs.length) return <>—</>;
  return <span className="monitoring-cost-lines">{costs.map(({ currency, cost }) => (
    <span key={currency} title={`${cost} ${currency}`}>{formatCurrency(cost, currency, locale)}</span>
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
  const age = formatElapsedTime(freshness.age_millis, locale);
  return <span title={occurred}>{t('monitoring.freshnessAge', { age })}</span>;
}

function MonitoringMetricList({ metrics }: { metrics: OperatorMonitoringSnapshot['summary'] }) {
  const { locale, t } = useI18n();
  const terminal = metrics.successful_requests + metrics.failed_requests;
  const successRate = terminal > 0 ? metrics.successful_requests / terminal : null;
  return <dl className="monitoring-metric-list">
    <div><dt>{t('usage.requests')}</dt><dd>{formatNumber(metrics.requests, locale)}</dd></div>
    <div><dt>{t('usage.successRate')}</dt><dd>{formatPercent(successRate, locale)}</dd></div>
    <div><dt>{t('usage.average')}</dt><dd>{formatMilliseconds(metrics.avg_duration_ms, locale)}</dd></div>
    <div><dt>{t('usage.p95Approx')}</dt><dd>{formatMilliseconds(metrics.p95_duration_ms, locale)}</dd></div>
    <div><dt>{t('traffic.cost')}</dt><dd><CostLines costs={metrics.costs} /></dd></div>
  </dl>;
}

export function MonitoringSnapshot({ snapshot }: { snapshot: OperatorMonitoringSnapshot }) {
  const { locale, t } = useI18n();
  const summary = snapshot.summary;
  const successRate = summary.requests > 0 ? summary.successful_requests / summary.requests : null;
  const range = `${new Date(snapshot.from_created_at).toLocaleString(locale)} – ${new Date(snapshot.to_created_at).toLocaleString(locale)}`;
  return <section className="operator-monitoring" aria-labelledby="monitoring-heading">
    <article className="panel">
      <div className="panel-title monitoring-heading">
        <div><h2 id="monitoring-heading">{t('monitoring.title')}</h2><p className="muted">{range}</p></div>
        <RoutingStatusBadge health={snapshot.health} />
      </div>
      <section className="metrics operator-monitoring-metrics monitoring-metrics-grid" aria-label={t('monitoring.summary')}>
        <NumberMetric label={t('usage.requests')} value={summary.requests} />
        <NumberMetric label={t('traffic.success')} value={summary.successful_requests} tone="positive" />
        <NumberMetric label={t('traffic.failure')} value={summary.failed_requests} tone="negative" />
        <Metric label={t('usage.successRate')} value={formatPercent(successRate, locale)} tone="positive" />
        <Metric label={t('usage.average')} value={formatMilliseconds(summary.avg_duration_ms, locale)} />
        <Metric label={t('usage.p95Approx')} value={formatMilliseconds(summary.p95_duration_ms, locale)} />
        <Metric label={t('traffic.cost')} value={<CostLines costs={summary.costs} />} />
        <Metric label={t('monitoring.freshness')} value={<Freshness snapshot={snapshot} />} />
      </section>
    </article>
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
              <div><b>{value.upstream_name}</b><code title={value.upstream_account_id}>{value.upstream_account_id}</code></div>
              <RoutingStatusBadge health={value.health} />
            </div>
            <MonitoringMetricList metrics={metrics} />
            <ol className="monitoring-outcomes" aria-label={t('monitoring.terminalOutcomes')}>
              {value.terminal_outcomes.slice(0, 5).map((outcome) => <li key={`${outcome.source}\0${outcome.id}`}>
                <time dateTime={new Date(outcome.created_at).toISOString()}>{new Date(outcome.created_at).toLocaleString(locale)}</time>
                <span className={`status ${outcome.status === 'success' ? 'ok' : 'bad'}`}>{t(`monitoring.outcome.${outcome.status}`)}</span>
                <span>{t(`monitoring.source.${outcome.source}`)}</span>
                <span>{formatMilliseconds(outcome.duration_ms, locale)}</span>
                {outcome.error_code && <code>{outcome.error_code}</code>}
              </li>)}
            </ol>
          </li>;
          })}</ul>
        </li>)}</ul>}
    </article>
  </section>;
}
