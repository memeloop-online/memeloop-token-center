import { useEffect, useRef, useState } from 'react';
import { api } from '../api';
import { formatElapsedTime, formatNumber, formatPercent } from '../format';
import { useI18n } from '../i18n';
import { quotaUsedPercent, upstreamQuotaPath, type UpstreamQuotaSnapshot } from './upstreamQuota';
import './upstreamQuota.css';
import { UpstreamQuotaReset } from './UpstreamQuotaReset';

export function UpstreamQuotaDetails({ snapshot }: { snapshot: UpstreamQuotaSnapshot }) {
  const { locale, t } = useI18n();
  const [now, setNow] = useState(Date.now);
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 30_000);
    return () => window.clearInterval(timer);
  }, []);
  const stale = snapshot.stale || (snapshot.stale_after !== null && now >= snapshot.stale_after);
  const reset = snapshot.reset_capability;
  const resetMessage = reset.provider_supported === false ? 'quota.resetUnsupported'
    : reset.provider_supported === null ? 'quota.resetUnknown'
    : !reset.implementation_available ? 'quota.resetNotIntegrated' : 'quota.resetAvailable';
  return <div className="upstream-quota-details">
    <div className="upstream-quota-meta">
      {snapshot.plan_type && <b>{snapshot.plan_type}</b>}
      <span>{snapshot.observed_at === null ? t('quota.notObserved') : t('quota.observedAt', { time: new Date(snapshot.observed_at).toLocaleString(locale) })}</span>
      {stale && <span className="status pending">{t('quota.stale')}</span>}
      {snapshot.credits.balance !== null && <span>{t('quota.balance', { amount: snapshot.credits.balance })}</span>}
      {snapshot.credits.unlimited === true && <span>{t('quota.unlimitedCredits')}</span>}
    </div>
    {snapshot.status !== 'ready' && <p role={snapshot.status === 'error' ? 'alert' : undefined}>{t(snapshot.status === 'unsupported' ? 'quota.readUnsupported' : 'quota.readFailed')}</p>}
    {snapshot.status === 'ready' && snapshot.windows.length === 0 && <p>{t('quota.noWindows')}</p>}
    <div className="upstream-quota-windows">{snapshot.windows.map((window) => {
      const used = quotaUsedPercent(window);
      return <section className="upstream-quota-window" key={window.id}>
        <div className="upstream-quota-window-heading"><b>{window.label}</b><strong>{formatPercent(used === null ? null : used / 100, locale)}</strong></div>
        {used !== null && <meter min={0} max={100} value={Math.max(0, Math.min(100, used))} aria-label={t('quota.usedPercent', { name: window.label })} />}
        {window.limit_reached === true && <span className="status bad">{t('quota.limitReached')}</span>}
        {window.allowed === false && window.limit_reached !== true && <span className="status pending">{t('quota.notAllowed')}</span>}
        {window.remaining !== null && <p>{t('quota.remaining', { amount: formatNumber(window.remaining, locale), limit: window.limit === null ? '—' : formatNumber(window.limit, locale) })}</p>}
        <p>{window.reset_at === null ? t('quota.resetTimeUnknown') : t(window.reset_is_estimated ? 'quota.estimatedResetAt' : 'quota.resetAt', { time: new Date(window.reset_at).toLocaleString(locale) })}</p>
        {window.reset_at !== null && <span>{window.reset_at <= now ? t('quota.resetElapsed') : t('quota.resetIn', { time: formatElapsedTime(window.reset_at - now, locale) })}</span>}
        <small>{t('quota.source', { source: window.source })}</small>
      </section>;
    })}</div>
    <div className="upstream-quota-reset">
      <b>{t('quota.resetCapability')}</b>
      <p>{t(resetMessage)}</p>
      {reset.available_credits !== null && <span>{t('quota.resetCredits', { available: formatNumber(reset.available_credits, locale), applicable: reset.applicable_credits === null ? '—' : formatNumber(reset.applicable_credits, locale) })}</span>}
      {reset.credit_error_code && <p>{t('quota.resetCreditsUnavailable')}</p>}
    </div>
  </div>;
}

/** User-triggered read: never starts one upstream request per card on page load. */
export function UpstreamQuota({ accountId, accountName = accountId, tenant, token }: { accountId: string; accountName?: string; tenant: string; token: string }) {
  const { t } = useI18n();
  const [snapshot, setSnapshot] = useState<UpstreamQuotaSnapshot>();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(false);
  const scope = `${token}\0${tenant}\0${accountId}`;
  const scopeRef = useRef(scope);
  scopeRef.current = scope;
  const requestRef = useRef<AbortController | null>(null);
  useEffect(() => {
    setSnapshot(undefined); setBusy(false); setError(false);
    return () => requestRef.current?.abort();
  }, [scope]);
  async function load() {
    if (busy || !tenant || !token) return;
    const controller = new AbortController();
    requestRef.current?.abort();
    requestRef.current = controller;
    setBusy(true); setError(false);
    try {
      const value = await api<UpstreamQuotaSnapshot>(upstreamQuotaPath(accountId, tenant), token, { signal: AbortSignal.any([controller.signal, AbortSignal.timeout(20_000)]) });
      if (scopeRef.current !== scope || controller.signal.aborted) return;
      if (value.upstream_account_id !== accountId || value.tenant_external_id !== tenant || value.contract_version !== 'upstream_quota_v1') throw new Error('Quota scope mismatch');
      setSnapshot(value);
    } catch {
      if (scopeRef.current === scope && !controller.signal.aborted) setError(true);
    } finally {
      if (scopeRef.current === scope && !controller.signal.aborted) setBusy(false);
    }
  }
  return <section className="upstream-quota" aria-label={t('quota.title')} aria-busy={busy}>
    <div className="upstream-quota-heading"><h3>{t('quota.title')}</h3><button type="button" className="secondary" disabled={busy || !tenant} onClick={() => void load()}>{t(busy ? 'common.loading' : snapshot ? 'quota.refresh' : 'quota.view')}</button></div>
    {error && <p className="notice error" role="alert">{t(snapshot ? 'quota.refreshFailedRetained' : 'quota.readFailed')}</p>}
    {!snapshot && busy && <div className="upstream-quota-loading" role="status"><span>{t('common.loading')}</span><div className="upstream-quota-skeleton" aria-hidden="true"><i /><i /></div></div>}
    {!snapshot && !busy && !error && <p>{t(tenant ? 'quota.notLoaded' : 'quota.selectTenant')}</p>}
    {snapshot && <UpstreamQuotaDetails snapshot={snapshot} />}
    {snapshot && snapshot.reset_capability.provider_supported === true && snapshot.reset_capability.implementation_available && <details className="upstream-danger-zone"><summary>{t('quota.resetAction')}</summary><p>{t('quota.resetWarning')}</p><UpstreamQuotaReset key={scope} accountId={accountId} accountName={accountName} tenant={tenant} token={token} snapshot={snapshot} /></details>}
  </section>;
}
