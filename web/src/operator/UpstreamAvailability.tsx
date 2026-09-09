import { formatMilliseconds, formatNumber, formatPercent } from '../format';
import { useI18n } from '../i18n';
import type { MonitoringTerminalOutcome, OperatorMonitoringSnapshot, UpstreamAccount, UpstreamHealth } from '../types';
import { RoutingStatusBadge } from './MonitoringSnapshot';

type AccountAttempt = MonitoringTerminalOutcome & { model: string };

function accountAttempts(values: OperatorMonitoringSnapshot['top_upstream_models']): AccountAttempt[] {
  return values.flatMap((value) => value.terminal_outcomes.map((outcome) => ({ ...outcome, model: value.model })))
    .sort((left, right) => right.created_at - left.created_at || right.id.localeCompare(left.id))
    .slice(0, 5);
}

function ManualHealthCheck({ health }: { health?: UpstreamHealth }) {
  const { locale, t } = useI18n();
  return <section className="provider-manual-health" aria-label={t('providers.manualHealthCheck')}>
    <small>{t('providers.manualHealthCheck')}</small>
    {!health ? <span className="muted">{t('providers.manualHealthNotRun')}</span> : <div className="provider-manual-health-result">
      <span className={`status ${health.status === 'healthy' ? 'ok' : 'bad'}`}>{health.status === 'healthy' ? t('providers.healthy') : t('providers.unhealthy')}</span>
      <span>{t('providers.checkedAt', { time: new Date(health.checked_at).toLocaleString(locale) })}</span>
      {health.upstream_status !== undefined && <span>HTTP {formatNumber(health.upstream_status, locale)}</span>}
      {health.latency_ms !== undefined && <span>{formatMilliseconds(health.latency_ms, locale)}</span>}
      {health.error_code && <code>{health.error_code}</code>}
    </div>}
  </section>;
}

export function UpstreamAvailability({ account, snapshot, manualHealth, onOpenRequest }: {
  account: UpstreamAccount;
  snapshot?: OperatorMonitoringSnapshot;
  manualHealth?: UpstreamHealth;
  onOpenRequest?: (requestId: string) => void;
}) {
  const { locale, t } = useI18n();
  const models = snapshot?.top_upstream_models.filter((value) => value.upstream_account_id === account.id) ?? [];
  const attempts = accountAttempts(models);
  const range = snapshot ? `${new Date(snapshot.from_created_at).toLocaleString(locale)} – ${new Date(snapshot.to_created_at).toLocaleString(locale)}` : undefined;
  const expired = account.credential_expires_at !== null && account.credential_expires_at <= Date.now();

  return <section className="provider-availability" aria-label={t('providers.recentAvailability')}>
    <div className="provider-availability-heading">
      <div><b>{t('providers.recentAvailability')}</b>{range && <small>{t('providers.observationWindow', { range })}</small>}</div>
      <div className="provider-availability-states">
        <span className="provider-availability-state"><small>{t('providers.accountStatus')}</small><span className={`status ${account.status === 'active' ? 'ok' : 'pending'}`}>{account.status === 'active' ? t('status.active') : t('status.disabled')}</span></span>
        {account.credential_expires_at !== null && <span className="provider-availability-state"><small>{t('providers.credentialStatus')}</small><span className={`status ${expired ? 'bad' : 'pending'}`}>{expired ? t('providers.credentialExpired') : t('providers.credentialExpires', { time: new Date(account.credential_expires_at).toLocaleString(locale) })}</span></span>}
      </div>
    </div>
    {!snapshot ? <div className="provider-availability-empty">{t('providers.availabilityUnavailable')}</div> : models.length === 0 ? <div className="provider-availability-empty">{t('providers.noRecentAttempts')}</div> : <>
      <p className="provider-availability-scope">{t('providers.snapshotSamples')}</p>
      <div className="provider-availability-models">{models.map((value) => {
        const terminal = value.metrics.successful_requests + value.metrics.failed_requests;
        return <section className="provider-availability-model" key={value.model}>
          <div className="provider-availability-model-heading"><code>{value.model}</code><span className="provider-availability-state"><RoutingStatusBadge health={value.health} />{value.health.observed_at === null ? <small>{t('providers.noObservationTime')}</small> : <small>{t('providers.observedAt', { time: new Date(value.health.observed_at).toLocaleString(locale) })}</small>}</span></div>
          <dl className="provider-availability-metrics">
            <div><dt>{t('usage.requests')}</dt><dd>{formatNumber(value.metrics.requests, locale)}</dd></div>
            <div><dt>{t('usage.successRate')}</dt><dd>{formatPercent(terminal > 0 ? value.metrics.successful_requests / terminal : null, locale)}</dd></div>
            <div><dt>{t('usage.average')}</dt><dd>{formatMilliseconds(value.metrics.avg_duration_ms, locale)}</dd></div>
            <div><dt>{t('usage.p95Approx')}</dt><dd>{formatMilliseconds(value.metrics.p95_duration_ms, locale)}</dd></div>
          </dl>
        </section>;
      })}</div>
      <ol className="provider-recent-attempts" aria-label={t('providers.recentAttempts')}>
        {attempts.map((attempt) => <li key={`${attempt.source}\0${attempt.id}`}>
          <time dateTime={new Date(attempt.created_at).toISOString()}>{new Date(attempt.created_at).toLocaleString(locale)}</time>
          {attempt.source === 'request' && onOpenRequest
            ? <button type="button" className={`status provider-attempt-link ${attempt.status === 'success' ? 'ok' : 'bad'}`} aria-label={t('providers.openRequest', { id: attempt.id })} title={attempt.id} onClick={() => onOpenRequest(attempt.id)}>{t(`monitoring.outcome.${attempt.status}`)}</button>
            : <span className={`status ${attempt.status === 'success' ? 'ok' : 'bad'}`}>{t(`monitoring.outcome.${attempt.status}`)}</span>}
          <code title={attempt.model}>{attempt.model}</code>
          <span>{t(`monitoring.source.${attempt.source}`)}</span>
          <span>{formatMilliseconds(attempt.duration_ms, locale)}</span>
          {attempt.error_code && <code>{attempt.error_code}</code>}
        </li>)}
      </ol>
    </>}
    <ManualHealthCheck health={manualHealth} />
  </section>;
}
