import { formatMilliseconds, formatNumber, formatPercent } from '../format';
import { useI18n } from '../i18n';
import type { OperatorMonitoringSnapshot, UpstreamAccount, UpstreamHealth } from '../types';
import { RoutingStatusBadge } from './MonitoringSnapshot';
import type { UpstreamAvailabilityWindow } from './upstreamAvailabilityWindow';

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

export function UpstreamAvailability({ account, snapshot, window, manualHealth, onOpenRequest }: {
  account: UpstreamAccount;
  snapshot?: OperatorMonitoringSnapshot;
  window?: UpstreamAvailabilityWindow;
  manualHealth?: UpstreamHealth;
  onOpenRequest?: (requestId: string) => void;
}) {
  const { locale, t } = useI18n();
  const models = snapshot?.top_upstream_models.filter((value) => value.upstream_account_id === account.id) ?? [];
  const scopedWindow = window?.tenant_external_id === account.tenant_external_id ? window : undefined;
  const facts = scopedWindow?.accounts.find((value) => value.upstream_account_id === account.id);
  const attempts = facts?.terminal_outcomes ?? [];
  const range = scopedWindow ? `${new Date(scopedWindow.from_created_at).toLocaleString(locale)} – ${new Date(scopedWindow.to_created_at).toLocaleString(locale)}` : undefined;
  const terminal = facts ? facts.metrics.successful_requests + facts.metrics.failed_requests : 0;
  const expired = account.credential_expires_at !== null && account.credential_expires_at <= Date.now();

  return <section className="provider-availability" aria-label={t('providers.recentAvailability')}>
    <div className="provider-availability-heading">
      <div><b>{t('providers.recentAvailability')}</b>{range && <small>{t('providers.observationWindow', { range })}</small>}</div>
      <div className="provider-availability-states">
        <span className="provider-availability-state"><small>{t('providers.accountStatus')}</small><span className={`status ${account.status === 'active' ? 'ok' : 'pending'}`}>{account.status === 'active' ? t('status.active') : t('status.disabled')}</span></span>
        {account.credential_expires_at !== null && <span className="provider-availability-state"><small>{t('providers.credentialStatus')}</small><span className={`status ${expired ? 'bad' : 'pending'}`}>{expired ? t('providers.credentialExpired') : t('providers.credentialExpires', { time: new Date(account.credential_expires_at).toLocaleString(locale) })}</span></span>}
      </div>
    </div>
    {!facts ? <div className="provider-availability-empty">{t('providers.availabilityUnavailable')}</div> : <>
      <p className="provider-availability-scope">{t('providers.accountWindowScope')}</p>
      <dl className="provider-availability-metrics provider-account-metrics">
        <div><dt>{t('usage.requests')}</dt><dd>{formatNumber(facts.metrics.requests, locale)}</dd></div>
        <div><dt>{t('usage.successRate')}</dt><dd>{formatPercent(terminal > 0 ? facts.metrics.successful_requests / terminal : null, locale)}</dd></div>
        <div><dt>{t('usage.average')}</dt><dd>{formatMilliseconds(facts.metrics.avg_duration_ms, locale)}</dd></div>
        <div><dt>{t('usage.p95Approx')}</dt><dd>{formatMilliseconds(facts.metrics.p95_duration_ms, locale)}</dd></div>
      </dl>
      {attempts.length === 0 && <div className="provider-availability-empty">{t('providers.accountWindowEmpty')}</div>}
      <small>{t('providers.accountWindowUpdated', { time: new Date(scopedWindow!.generated_at).toLocaleString(locale) })}</small>
    </>}
    {models.length > 0 && <>
      <p className="provider-availability-scope">{t('providers.routingSampleScope')}</p>
      <div className="provider-availability-models">{models.map((value) => {
        return <section className="provider-availability-model" key={value.model}>
          <div className="provider-availability-model-heading"><code>{value.model}</code><span className="provider-availability-state"><RoutingStatusBadge health={value.health} />{value.health.observed_at === null ? <small>{t('providers.noObservationTime')}</small> : <small>{t('providers.observedAt', { time: new Date(value.health.observed_at).toLocaleString(locale) })}</small>}</span></div>
        </section>;
      })}</div>
    </>}
    {attempts.length > 0 && <>
      <h3 className="provider-attempts-title">{t('providers.accountWindowRecent')}</h3>
      <ol className="provider-recent-attempts" aria-label={t('providers.accountWindowRecent')}>
        {attempts.map((attempt) => <li key={`${attempt.source}\0${attempt.id}`}>
          <time dateTime={new Date(attempt.created_at).toISOString()}>{new Date(attempt.created_at).toLocaleString(locale)}</time>
          {attempt.source === 'request' && onOpenRequest
            ? <button type="button" className={`status provider-attempt-link ${attempt.status === 'success' ? 'ok' : 'bad'}`} aria-label={t('providers.openRequest', { id: attempt.id })} title={attempt.id} onClick={() => onOpenRequest(attempt.id)}>{t(`monitoring.outcome.${attempt.status}`)}</button>
            : <span className={`status ${attempt.status === 'success' ? 'ok' : 'bad'}`}>{t(`monitoring.outcome.${attempt.status}`)}</span>}
          <span>{t(`monitoring.source.${attempt.source}`)}</span>
          <span>{formatMilliseconds(attempt.duration_ms, locale)}</span>
          {attempt.error_code && <code>{attempt.error_code}</code>}
        </li>)}
      </ol>
    </>}
    <ManualHealthCheck health={manualHealth} />
  </section>;
}
