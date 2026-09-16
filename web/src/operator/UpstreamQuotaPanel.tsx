import { useEffect, useRef, useState } from 'react';
import { api, ApiError } from '../api';
import { formatCountdown, formatNumber, formatPercent } from '../format';
import { useI18n } from '../i18n';
import { quotaObservationState, quotaReadErrorMessage, quotaRemaining, quotaResetCreditExpiry, quotaSourceLabel, quotaUnitMessage, quotaUsedPercent, upstreamQuotaPath, type UpstreamQuotaSnapshot } from './upstreamQuota';
import { useQuotaWindowLabel } from './QuotaSummary';
import { useQuotaClock } from './useQuotaClock';
import type { QuotaReadState } from './useUpstreamQuotaReads';
import './upstreamQuota.css';
import { UpstreamQuotaReset } from './UpstreamQuotaReset';
import { DetailTooltip } from '../design-system';

export function UpstreamQuotaDetails({ snapshot, refreshError }: { snapshot: UpstreamQuotaSnapshot; refreshError?: 'quota.readFailed' | 'quota.errorPermission' }) {
  const { locale, t } = useI18n();
  const windowLabel = useQuotaWindowLabel();
  const now = useQuotaClock();
  const observation = quotaObservationState(snapshot, now, Boolean(refreshError));
  const hasObservation = observation !== 'unobserved';
  const readFailed = Boolean(refreshError) || snapshot.status === 'error' || Boolean(snapshot.error_code);
  return <div className="upstream-quota-details">
    <div className="upstream-quota-meta">
      {hasObservation && snapshot.plan_type && <b>{snapshot.plan_type}</b>}
      <span>{snapshot.observed_at === null ? t('quota.notObserved') : t(observation === 'historical' ? 'quota.lastObservedAt' : 'quota.observedAt', { time: new Date(snapshot.observed_at).toLocaleString(locale) })}</span>
      {observation === 'historical' && <span className="status pending">{t('quota.historical')}</span>}
      {hasObservation && snapshot.credits.source === 'codex_usage' && typeof snapshot.credits.balance === 'string' && snapshot.credits.balance.trim() !== '' && <span>{t(observation === 'historical' ? 'quota.lastObservedBalance' : 'quota.balance', { amount: snapshot.credits.balance })}</span>}
      {hasObservation && snapshot.credits.source === 'codex_usage' && snapshot.credits.unlimited === true && <span>{t(observation === 'historical' ? 'quota.lastObservedUnlimitedCredits' : 'quota.unlimitedCredits')}</span>}
    </div>
    {snapshot.status === 'unsupported' && <p>{t('quota.readUnsupported')}</p>}
    {(snapshot.status !== 'unsupported' || refreshError) && readFailed && <div className="notice error" role="alert">
      <p>{t(refreshError ?? quotaReadErrorMessage(snapshot.error_code))}</p>
      <p>{snapshot.observed_at === null ? t('quota.refreshFailedNoObservation') : t('quota.refreshFailedRetainedAt', { time: new Date(snapshot.observed_at).toLocaleString(locale) })}</p>
    </div>}
    {hasObservation && snapshot.status === 'ready' && snapshot.windows.length === 0 && <p>{t(observation === 'historical' ? 'quota.noWindowsHistorical' : 'quota.noWindows')}</p>}
    {hasObservation && <div className="upstream-quota-windows">{snapshot.windows.map((window) => {
      const used = quotaUsedPercent(window);
      const remaining = quotaRemaining(window);
      const unitMessage = remaining?.kind === 'amount' ? quotaUnitMessage(remaining.unit) : null;
      const name = windowLabel(snapshot.provider, window);
      const source = t(quotaSourceLabel(window.source));
      return <section className="upstream-quota-window" key={window.id}>
        <div className="upstream-quota-window-heading"><div><b>{name}</b>{observation === 'historical' && <small>{t('quota.lastObservedValue')}</small>}</div><strong>{formatPercent(used === null ? null : used / 100, locale)}</strong></div>
        {used !== null && <meter min={0} max={100} value={Math.max(0, Math.min(100, used))} aria-label={t(observation === 'historical' ? 'quota.lastObservedUsedPercent' : 'quota.usedPercent', { name })} />}
        {window.limit_reached === true && <span className="status bad">{t('quota.limitReached')}</span>}
        {window.allowed === false && window.limit_reached !== true && <span className="status pending">{t('quota.notAllowed')}</span>}
        {remaining && <p>{remaining.kind === 'percent'
          ? t('quota.remainingPercent', { percent: formatPercent(remaining.percent / 100, locale) })
          : t('quota.remainingWithUnit', { amount: formatNumber(remaining.amount, locale), limit: remaining.limit === null ? '—' : formatNumber(remaining.limit, locale), unit: unitMessage ? t(unitMessage) : remaining.unit })}</p>}
        <p>{window.reset_at === null ? t('quota.resetTimeUnknown') : t(window.reset_is_estimated ? 'quota.estimatedResetAt' : 'quota.resetAt', { time: new Date(window.reset_at).toLocaleString(locale) })}</p>
        {window.reset_at !== null && <span>{window.reset_at <= now ? t('quota.resetElapsed') : t('quota.resetIn', { time: formatCountdown(window.reset_at - now, locale) })}</span>}
        <DetailTooltip content={t('quota.sourceEvidence', { source, id: window.id, rawSource: window.source })}><small tabIndex={0} data-quota-evidence={window.id}>{t('quota.sourceLabel')}</small></DetailTooltip>
      </section>;
    })}</div>}
  </div>;
}

export function QuotaResetCreditExpiry({ snapshot, now }: { snapshot: UpstreamQuotaSnapshot; now?: number }) {
  const { t, locale } = useI18n();
  const clock = useQuotaClock();
  const expiry = quotaResetCreditExpiry(snapshot, now ?? clock);
  return <span data-reset-credit-expiry={expiry.state}>{t(expiry.state === 'known' ? 'quota.creditExpiresAt' : expiry.state === 'none' ? 'quota.noUnexpiredCredits' : 'quota.creditExpiryUnknown', { time: expiry.at === undefined ? '—' : new Date(expiry.at).toLocaleString(locale) })}</span>;
}

/** Capability discovery must remain visible even when the first read fails.
 * An absent snapshot is unknown, never evidence that a supplier supports reset.
 * Rendering without a snapshot performs no request. An actionable snapshot may
 * recover durable local reset state, but never reads supplier quota or mutates it.
 */
export function UpstreamQuotaResetSection({ accountId, accountName, tenant, token, snapshot, readFailed = false }: {
  accountId: string; accountName: string; tenant: string; token: string;
  snapshot?: UpstreamQuotaSnapshot; readFailed?: boolean;
}) {
  const { t, locale } = useI18n();
  const capability = snapshot?.reset_capability;
  if (capability?.provider_supported === false
    || (snapshot?.status === 'unsupported' && !capability?.implementation_available)) return null;
  const resetMessage = capability?.provider_supported == null ? 'quota.resetUnknown'
    : !capability.implementation_available ? 'quota.resetNotIntegrated' : 'quota.resetAvailable';
  const actionable = capability?.provider_supported === true && capability.implementation_available;
  return <section className="upstream-quota-reset" aria-label={t('quota.resetCapability')}>
    <h3>{t('quota.resetCapability')}</h3>
    {snapshot ? <>
      <p>{t(resetMessage)}</p>
      {capability?.available_credits !== null && capability?.available_credits !== undefined && <span>{t('quota.resetCredits', { available: formatNumber(capability.available_credits, locale), applicable: capability.applicable_credits === null ? '—' : formatNumber(capability.applicable_credits, locale) })}</span>}
      {snapshot.provider === 'openai-codex' && <QuotaResetCreditExpiry snapshot={snapshot} />}
      {capability?.credit_error_code && <p>{t('quota.resetCreditsUnavailable')} {t(quotaReadErrorMessage(capability.credit_error_code))}</p>}
      {actionable && <UpstreamQuotaReset accountId={accountId} accountName={accountName} tenant={tenant} token={token} snapshot={snapshot} />}
    </> : <>
      <p role="status">{t(readFailed ? 'quota.resetDiscoveryFailed' : 'quota.resetDiscoveryPending')}</p>
      <button type="button" className="danger" disabled>{t('quota.resetAction')}</button>
    </>}
  </section>;
}

/** User-triggered read: never starts one upstream request per card on page load. */
export function UpstreamQuota({ accountId, accountName = accountId, credentialGeneration, tenant, token, onSnapshot, onRefreshFailed, initialSnapshot, readState, onRefresh, refreshDisabled = false }: { accountId: string; accountName?: string; credentialGeneration: number; tenant: string; token: string; onSnapshot?: (snapshot: UpstreamQuotaSnapshot) => void; onRefreshFailed?: () => void; initialSnapshot?: UpstreamQuotaSnapshot; readState?: QuotaReadState; onRefresh?: () => void; refreshDisabled?: boolean }) {
  const { t } = useI18n();
  const [localSnapshot, setSnapshot] = useState<UpstreamQuotaSnapshot | undefined>(initialSnapshot);
  const [localBusy, setBusy] = useState(false);
  const [localError, setError] = useState<'quota.readFailed' | 'quota.errorPermission'>();
  const snapshot = onRefresh ? readState?.snapshot : localSnapshot;
  const busy = onRefresh ? Boolean(readState?.busy) : localBusy;
  const error = onRefresh ? readState?.error : localError;
  const scope = `${token}\0${tenant}\0${accountId}\0${credentialGeneration}`;
  const scopeRef = useRef(scope);
  scopeRef.current = scope;
  const requestRef = useRef<AbortController | null>(null);
  useEffect(() => {
    setSnapshot(initialSnapshot); setBusy(false); setError(undefined);
    return () => requestRef.current?.abort();
  }, [scope]);
  async function load() {
    if (onRefresh) { onRefresh(); return; }
    if (busy || !tenant || !token) return;
    const controller = new AbortController();
    requestRef.current?.abort();
    requestRef.current = controller;
    setBusy(true); setError(undefined);
    try {
      const value = await api<UpstreamQuotaSnapshot>(upstreamQuotaPath(accountId, tenant), token, { signal: AbortSignal.any([controller.signal, AbortSignal.timeout(45_000)]) });
      if (scopeRef.current !== scope || controller.signal.aborted) return;
      if (value.upstream_account_id !== accountId || value.tenant_external_id !== tenant || value.contract_version !== 'upstream_quota_v1') throw new Error('Quota scope mismatch');
      setSnapshot(value);
      onSnapshot?.(value);
    } catch (reason) {
      if (scopeRef.current === scope && !controller.signal.aborted) {
        setError(reason instanceof ApiError && [401, 403].includes(reason.status) ? 'quota.errorPermission' : 'quota.readFailed');
        onRefreshFailed?.();
      }
    } finally {
      if (scopeRef.current === scope && !controller.signal.aborted) setBusy(false);
    }
  }
  return <section className="upstream-quota" aria-label={t('quota.title')} aria-busy={busy}>
    <div className="upstream-quota-heading"><h3>{t('quota.title')}</h3><button type="button" className="secondary" disabled={busy || !tenant || refreshDisabled} onClick={() => void load()}>{t(busy ? 'common.loading' : snapshot ? 'quota.refresh' : 'quota.view')}</button></div>
    {error && !snapshot && <div className="notice error" role="alert"><p>{t(error)}</p></div>}
    {!snapshot && busy && <div className="upstream-quota-loading" role="status"><span>{t('common.loading')}</span><div className="upstream-quota-skeleton" aria-hidden="true"><i /><i /></div></div>}
    {!snapshot && !busy && !error && <p>{t(tenant ? 'quota.notLoaded' : 'quota.selectTenant')}</p>}
    {snapshot && <UpstreamQuotaDetails snapshot={snapshot} refreshError={error} />}
    <UpstreamQuotaResetSection key={scope} accountId={accountId} accountName={accountName} tenant={tenant} token={token} snapshot={snapshot} readFailed={Boolean(error)} />
  </section>;
}
