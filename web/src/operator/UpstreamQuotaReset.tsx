import { useEffect, useRef, useState } from 'react';
import { api, ApiError } from '../api';
import { formatCountdown } from '../format';
import { useI18n } from '../i18n';
import { useConfirmDialog } from '../useConfirmDialog';
import { UPSTREAM_QUOTA_READ_TIMEOUT_MILLIS, type UpstreamQuotaSnapshot } from './upstreamQuota';
import { useQuotaClock } from './useQuotaClock';
import { Button, Disclosure } from '../design-system';

export interface QuotaResetOperation {
  id: string;
  upstream_account_id: string;
  state: 'prepared' | 'submitted' | 'accepted' | 'unknown' | 'expired';
  expires_at: number;
  effect: 'supplier_defined_codex_rate_limits';
  consumes_credits: number;
  last_reconciled_at: number | null;
  reconciled_available_credits: number | null;
  reconciled_applicable_credits: number | null;
  settled_at: number | null;
  updated_at?: number;
  manual_unlock_available?: boolean;
  quota_refresh_status?: 'not_attempted' | 'pending' | 'succeeded';
  dispatch_diagnostic?: { stage: 'prepare' | 'send' | 'response'; reason: string; supplier_http_status: number | null } | null;
}

export function UpstreamQuotaReset({ accountId, accountName, tenant, token, snapshot, canRecover = false, onQuotaChanged }: {
  accountId: string; accountName: string; tenant: string; token: string; snapshot: UpstreamQuotaSnapshot; canRecover?: boolean; onQuotaChanged?: () => void;
}) {
  const { locale, t } = useI18n();
  const now = useQuotaClock();
  const scope = `${token}\0${tenant}\0${accountId}`;
  const identity = useRef(scope);
  const secret = useRef<string | null>(null);
  if (identity.current !== scope) { identity.current = scope; secret.current = null; }
  const alive = useRef(true);
  const working = useRef(false);
  const prepareIdempotencyKey = useRef<string | null>(null);
  const confirmIdempotencyKey = useRef<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [operation, setOperation] = useState<QuotaResetOperation>();
  const [settled, setSettled] = useState<{ available: number | null; applicable: number | null }>();
  const [recoveryError, setRecoveryError] = useState<'stale' | 'forbidden' | 'unavailable'>();
  const [released, setReleased] = useState(false);
  const [error, setError] = useState<'prepare-rejected' | 'prepare-unavailable' | 'dispatch-unknown' | 'status-unavailable'>();
  const { confirm, confirmationDialog } = useConfirmDialog([token, tenant, accountId]);
  const base = `/internal/v1/upstreams/${encodeURIComponent(accountId)}/quota-reset`;
  const path = (suffix: string) => `${base}/${suffix}?${new URLSearchParams({ tenant_external_id: tenant })}`;
  const currentPath = `${base}/current?${new URLSearchParams({ tenant_external_id: tenant })}`;
  const valid = (value: QuotaResetOperation) => value.upstream_account_id === accountId && Boolean(value.id);
  useEffect(() => {
    const expectedScope = scope;
    const controller = new AbortController();
    alive.current = true; secret.current = null; working.current = true;
    prepareIdempotencyKey.current = null; confirmIdempotencyKey.current = null;
    setOperation(undefined); setBusy(true); setError(undefined); setSettled(undefined); setReleased(false); setRecoveryError(undefined);
    void api<QuotaResetOperation | null>(currentPath, token, { signal: AbortSignal.any([controller.signal, AbortSignal.timeout(20_000)]) })
      .then((value) => {
        if (!alive.current || identity.current !== expectedScope || !value) return;
        if (value.upstream_account_id !== accountId || !value.id) throw new Error('Invalid reset scope');
        acceptOperation(value);
      })
      .catch(() => { if (alive.current && identity.current === expectedScope && !controller.signal.aborted) setError('status-unavailable'); })
      .finally(() => {
        if (alive.current && identity.current === expectedScope) {
          working.current = false;
          setBusy(false);
        }
      });
    return () => { controller.abort(); alive.current = false; secret.current = null; prepareIdempotencyKey.current = null; confirmIdempotencyKey.current = null; };
  }, [scope, currentPath, token, accountId]);
  const current = () => alive.current && identity.current === scope;
  function acceptOperation(response: QuotaResetOperation | null) {
    if (!response || response.state === 'expired' || response.state === 'accepted' && response.settled_at != null) {
      secret.current = null; prepareIdempotencyKey.current = null; confirmIdempotencyKey.current = null;
      setOperation(undefined);
      if (response?.settled_at != null) setSettled({ available: response.reconciled_available_credits, applicable: response.reconciled_applicable_credits });
    } else setOperation(response);
  }
  // Poll only the local operation record. Never retry a supplier reset.
  useEffect(() => {
    if (!operation || operation.settled_at != null || operation.state === 'unknown') return;
    const timer = window.setInterval(() => { void reloadCurrent(); }, 5_000);
    return () => window.clearInterval(timer);
  }, [scope, operation?.id, operation?.state, operation?.settled_at]);
  async function reloadCurrent() {
    if (working.current || !current()) return;
    working.current = true; setBusy(true);
    try {
      const response = await api<QuotaResetOperation | null>(currentPath, token, { cache: 'no-store', signal: AbortSignal.timeout(20_000) });
      if (!current()) return;
      if (response && !valid(response)) throw new Error('Invalid reset scope');
      acceptOperation(response); setError(undefined); setRecoveryError(undefined);
      if (operation && (!response || response.state !== operation.state || response.settled_at !== operation.settled_at)) onQuotaChanged?.();
    } catch { if (current()) setError('status-unavailable'); }
    finally { if (current()) { working.current = false; setBusy(false); } }
  }
  async function dispatchPrepared(prepared: QuotaResetOperation, confirmationToken: string) {
    setOperation({ ...prepared, state: 'submitted' });
    confirmIdempotencyKey.current ??= `quota-confirm-${crypto.randomUUID()}`;
    const response = await api<QuotaResetOperation>(path(`${encodeURIComponent(prepared.id)}/confirm`), token, { method: 'POST', headers: { 'Idempotency-Key': confirmIdempotencyKey.current }, body: JSON.stringify({ confirmation_token: confirmationToken, confirmation: 'consume_one_supplier_reset_credit' }) });
    if (!current()) return;
    if (!valid(response) || response.id !== prepared.id) throw new Error('Invalid reset scope');
    secret.current = null;
    acceptOperation(response);
    onQuotaChanged?.();
  }
  async function prepare() {
    if (working.current || operation || error === 'status-unavailable' || !current()) return;
    working.current = true; setBusy(true); setError(undefined); setSettled(undefined);
    prepareIdempotencyKey.current ??= `quota-prepare-${crypto.randomUUID()}`;
    try {
      const result = await api<{ operation: QuotaResetOperation; confirmation_token: string; idempotency_replayed: boolean }>(path('prepare'), token, { method: 'POST', headers: { 'Idempotency-Key': prepareIdempotencyKey.current }, signal: AbortSignal.timeout(UPSTREAM_QUOTA_READ_TIMEOUT_MILLIS) });
      if (!current()) return;
      if (!valid(result.operation) || result.operation.effect !== 'supplier_defined_codex_rate_limits' || result.operation.consumes_credits !== 1) throw new Error('Invalid reset contract');
      if (result.operation.state === 'expired') { prepareIdempotencyKey.current = null; throw new Error('Reset preparation expired'); }
      if (result.operation.state !== 'prepared') throw new Error('Invalid reset state');
      setOperation(result.operation);
      secret.current = result.confirmation_token;
      const accepted = await confirm(t('quota.resetConfirm', { account: accountName, id: accountId, expiry: new Date(result.operation.expires_at).toLocaleString(locale) }));
      if (!current()) return;
      const confirmationToken = secret.current;
      if (!accepted || !confirmationToken || result.operation.expires_at <= Date.now()) return;
      // The durable idempotency key makes an exact user-requested retry safe;
      // this component never retries a credit-consuming request automatically.
      try { await dispatchPrepared(result.operation, confirmationToken); }
      catch { if (current()) setError('dispatch-unknown'); }
    } catch (reason) {
      // Preparation never dispatches the supplier credit-consuming request.
      // A timeout may still have created the same idempotent prepared record,
      // so keep the key and let this exact button safely recover it.
      if (current()) setError(reason instanceof ApiError && reason.status === 409 ? 'prepare-rejected' : 'prepare-unavailable');
    } finally {
      if (current()) { working.current = false; setBusy(false); }
    }
  }
  async function recover() {
    if (!current() || working.current || !canRecover || operation?.state !== 'unknown' || operation.settled_at != null) return;
    const reviewed = operation;
    working.current = true; setBusy(true); setRecoveryError(undefined);
    try {
      if (!await confirm(t('quota.recoverConfirm')) || !current()) return;
      const response = await api<QuotaResetOperation>(path(`${encodeURIComponent(reviewed.id)}/recover`), token, {
        method: 'POST', body: JSON.stringify({ confirmation: 'release_unknown_reset_without_consuming' }),
      });
      if (!current()) return;
      if (!valid(response) || response.id !== reviewed.id || response.state !== 'unknown' || response.settled_at == null) throw new Error('Invalid recovery scope');
      secret.current = null; prepareIdempotencyKey.current = null; confirmIdempotencyKey.current = null;
      setOperation(response); setReleased(true); onQuotaChanged?.();
    } catch (reason) {
      if (current()) setRecoveryError(reason instanceof ApiError && reason.status === 409 ? 'stale' : reason instanceof ApiError && [401, 403].includes(reason.status) ? 'forbidden' : 'unavailable');
    } finally { if (current()) { working.current = false; setBusy(false); } }
  }
  async function retryConfirmation() {
    const confirmationToken = secret.current;
    if (!operation || operation.state !== 'prepared' || !confirmationToken || operation.expires_at <= Date.now() || working.current) return;
    working.current = true; setBusy(true); setError(undefined);
    try {
      const accepted = await confirm(t('quota.resetConfirm', { account: accountName, id: accountId, expiry: new Date(operation.expires_at).toLocaleString(locale) }));
      if (accepted && current() && operation.expires_at > Date.now()) await dispatchPrepared(operation, confirmationToken);
    }
    catch { if (current()) setError('dispatch-unknown'); }
    finally { if (current()) { working.current = false; setBusy(false); } }
  }
  async function inspect(reconcile: boolean) {
    if (!operation || working.current) return;
    working.current = true; setBusy(true); setError(undefined);
    try {
      const response = await api<QuotaResetOperation>(path(`${encodeURIComponent(operation.id)}${reconcile ? '/reconcile' : ''}`), token, { ...(reconcile ? { method: 'POST' } : {}), signal: AbortSignal.timeout(UPSTREAM_QUOTA_READ_TIMEOUT_MILLIS) });
      if (!current()) return;
      if (!valid(response) || response.id !== operation.id) throw new Error('Invalid reset scope');
      if (reconcile) onQuotaChanged?.();
      if (response.state === 'unknown' && response.settled_at != null) {
        secret.current = null; prepareIdempotencyKey.current = null; confirmIdempotencyKey.current = null;
        setOperation(response); setReleased(true); return;
      }
      if (response.state === 'expired' || response.settled_at != null) {
        secret.current = null; prepareIdempotencyKey.current = null; confirmIdempotencyKey.current = null;
        setSettled(response.settled_at != null ? {
          available: response.reconciled_available_credits,
          applicable: response.reconciled_applicable_credits,
        } : undefined);
        setOperation(undefined);
        return;
      }
      if (response.state !== 'prepared') secret.current = null;
      setOperation(response);
      setRecoveryError(undefined);
    } catch { if (current()) setError('status-unavailable'); }
    finally { if (current()) { working.current = false; setBusy(false); } }
  }
  const capability = snapshot.reset_capability;
  if (capability.provider_supported !== true || !capability.implementation_available) return null;
  const dispatchState = operation && ['submitted', 'accepted', 'unknown'].includes(operation.state);
  const unlocked = released || operation?.state === 'unknown' && operation.settled_at != null;
  const recoverable = !unlocked && operation?.state === 'unknown' && operation.settled_at == null && canRecover;
  const operationDetails = operation && <>
    <code>{operation.id}</code>
    {operation.state === 'prepared' && <p data-reset-operation-expiry={operation.expires_at}>{t('quota.resetPreparedExpiresAt', { time: new Date(operation.expires_at).toLocaleString(locale) })} · {operation.expires_at <= now
      ? t('quota.resetPreparedExpired')
      : t('quota.resetPreparedExpiresIn', { time: formatCountdown(operation.expires_at - now, locale) })}</p>}
    {unlocked && <p>{t('quota.recoverReleased')}</p>}
    {operation.quota_refresh_status && operation.quota_refresh_status !== 'pending' && <p>{t(operation.quota_refresh_status === 'succeeded' ? 'quota.resetReadbackSucceeded' : 'quota.resetReadbackNotAttempted')}</p>}
    {operation.dispatch_diagnostic && <p>{t(['timeout', 'connect_timeout', 'connect_failed'].includes(operation.dispatch_diagnostic.reason) ? 'quota.resetNetworkUnknown' : 'quota.resetDispatchUnknown')}</p>}
    {operation.last_reconciled_at !== null && <p>{t('quota.resetReconciled', { time: new Date(operation.last_reconciled_at).toLocaleString(locale), available: operation.reconciled_available_credits ?? '—', applicable: operation.reconciled_applicable_credits ?? '—' })}</p>}
  </>;
  return <div className="upstream-quota-reset-action">
    {confirmationDialog}
    {!operation && <button type="button" className="danger" disabled={busy || !capability.prepare_available || error === 'status-unavailable'} onClick={() => void prepare()}>{t('quota.resetAction')}</button>}
    {settled && <p role="status" title={t('quota.resetSettledDetail')}>{t('quota.resetSettled', {
      available: settled.available ?? '—', applicable: settled.applicable ?? '—',
    })}</p>}
    {error && <p role="alert">{t(error === 'prepare-rejected' ? 'quota.resetPrepareRejected' : error === 'prepare-unavailable' ? 'quota.resetPrepareUnavailable' : error === 'dispatch-unknown' ? 'quota.resetOperationError' : 'quota.resetStatusUnavailable')}</p>}
    {!operation && error === 'status-unavailable' && <Button disabled={busy} onClick={() => void reloadCurrent()}>{t('quota.resetCheckStatus')}</Button>}
    {recoveryError && <p role="alert">{t(recoveryError === 'stale' ? 'quota.recoverStale' : recoveryError === 'forbidden' ? 'quota.recoverForbidden' : 'quota.recoverUnavailable')}</p>}
    {operation && <div role="status">
      <p>{t(unlocked ? 'quota.recoverReady' : recoverable ? 'quota.recoverHelp' : operation.state === 'accepted' ? 'quota.resetAccepted' : operation.state === 'prepared' ? secret.current ? 'quota.resetPrepared' : 'quota.resetPreparedWaiting' : operation.state === 'unknown' && !canRecover ? 'quota.resetAskOperator' : 'quota.resetUncertain')}</p>
      {operation.quota_refresh_status === 'pending' && <p>{t('quota.resetReadbackPending')}</p>}
      <Disclosure title={t('quota.resetOperationDetails')}>{operationDetails}</Disclosure>
      <div className="button-row">
        {(recoveryError || error || operation.state === 'prepared' && !secret.current) && <button type="button" className="secondary" disabled={busy} onClick={() => void inspect(false)}>{t('quota.resetCheckStatus')}</button>}
        {operation.state === 'prepared' && secret.current && <button type="button" className="danger" disabled={busy || operation.expires_at <= now} onClick={() => void retryConfirmation()}>{t('quota.resetRetryConfirmation')}</button>}
        {dispatchState && !recoverable && !unlocked && <button type="button" className="secondary" disabled={busy} onClick={() => void inspect(true)}>{t('quota.resetReconcile')}</button>}
      </div>
      {recoverable && <Button appearance="secondary" disabled={busy || recoveryError === 'stale' || recoveryError === 'forbidden'} onClick={() => void recover()}>{t('quota.recoverAction')}</Button>}
      {unlocked && <Button appearance="primary" disabled={busy} onClick={() => { setOperation(undefined); setReleased(false); setRecoveryError(undefined); }}>{t('quota.recoverContinue')}</Button>}
    </div>}
  </div>;
}
