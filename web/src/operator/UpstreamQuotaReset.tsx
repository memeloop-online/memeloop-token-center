import { useEffect, useRef, useState } from 'react';
import { api } from '../api';
import { useI18n } from '../i18n';
import { useConfirmDialog } from '../useConfirmDialog';
import type { UpstreamQuotaSnapshot } from './upstreamQuota';

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
}

export function UpstreamQuotaReset({ accountId, accountName, tenant, token, snapshot }: {
  accountId: string; accountName: string; tenant: string; token: string; snapshot: UpstreamQuotaSnapshot;
}) {
  const { locale, t } = useI18n();
  const scope = `${token}\0${tenant}\0${accountId}`;
  const identity = useRef(scope);
  const secret = useRef<string | null>(null);
  if (identity.current !== scope) { identity.current = scope; secret.current = null; }
  const alive = useRef(true);
  const working = useRef(false);
  const [busy, setBusy] = useState(false);
  const [operation, setOperation] = useState<QuotaResetOperation>();
  const [error, setError] = useState(false);
  const [attempted, setAttempted] = useState(false);
  const { confirm, confirmationDialog } = useConfirmDialog([token, tenant, accountId]);
  useEffect(() => {
    alive.current = true; secret.current = null; working.current = false;
    setOperation(undefined); setBusy(false); setError(false); setAttempted(false);
    return () => { alive.current = false; secret.current = null; };
  }, [scope]);
  const current = () => alive.current && identity.current === scope;
  const base = `/internal/v1/upstreams/${encodeURIComponent(accountId)}/quota-reset`;
  const path = (suffix: string) => `${base}/${suffix}?${new URLSearchParams({ tenant_external_id: tenant })}`;
  const valid = (value: QuotaResetOperation) => value.upstream_account_id === accountId && Boolean(value.id);
  async function prepare() {
    if (working.current || operation || attempted || !current()) return;
    working.current = true; setBusy(true); setError(false); setAttempted(true);
    try {
      const result = await api<{ operation: QuotaResetOperation; confirmation_token: string }>(path('prepare'), token, { method: 'POST', signal: AbortSignal.timeout(20_000) });
      if (!current()) return;
      if (!valid(result.operation) || result.operation.state !== 'prepared' || result.operation.effect !== 'supplier_defined_codex_rate_limits' || result.operation.consumes_credits !== 1) throw new Error('Invalid reset contract');
      setOperation(result.operation);
      secret.current = result.confirmation_token;
      const accepted = await confirm(t('quota.resetConfirm', { account: accountName, id: accountId, expiry: new Date(result.operation.expires_at).toLocaleString(locale) }));
      if (!current()) return;
      const confirmationToken = secret.current;
      secret.current = null;
      if (!accepted || !confirmationToken || result.operation.expires_at <= Date.now()) return;
      // Lock before dispatch; a timeout is unknown, never permission to retry.
      setOperation({ ...result.operation, state: 'submitted' });
      const response = await api<QuotaResetOperation>(path(`${encodeURIComponent(result.operation.id)}/confirm`), token, { method: 'POST', body: JSON.stringify({ confirmation_token: confirmationToken }), signal: AbortSignal.timeout(20_000) });
      if (!current()) return;
      if (!valid(response) || response.id !== result.operation.id) throw new Error('Invalid reset scope');
      setOperation(response);
    } catch {
      if (current()) setError(true);
    } finally {
      secret.current = null;
      if (current()) { working.current = false; setBusy(false); }
    }
  }
  async function inspect(reconcile: boolean) {
    if (!operation || working.current) return;
    working.current = true; setBusy(true); setError(false);
    try {
      const response = await api<QuotaResetOperation>(path(`${encodeURIComponent(operation.id)}${reconcile ? '/reconcile' : ''}`), token, { ...(reconcile ? { method: 'POST' } : {}), signal: AbortSignal.timeout(20_000) });
      if (!current()) return;
      if (!valid(response) || response.id !== operation.id) throw new Error('Invalid reset scope');
      setOperation(response);
    } catch { if (current()) setError(true); }
    finally { if (current()) { working.current = false; setBusy(false); } }
  }
  const capability = snapshot.reset_capability;
  if (capability.provider_supported !== true || !capability.implementation_available) return null;
  const dispatchState = operation && ['submitted', 'accepted', 'unknown'].includes(operation.state);
  return <div className="upstream-quota-reset-action">
    {confirmationDialog}
    {!operation && <button type="button" className="danger" disabled={busy || attempted || capability.credit_error_code !== null || (capability.applicable_credits ?? 0) < 1} onClick={() => void prepare()}>{t('quota.resetAction')}</button>}
    {error && <p role="alert">{t('quota.resetOperationError')}</p>}
    {operation && <div role="status">
      <p>{t(dispatchState ? operation.state === 'accepted' ? 'quota.resetAccepted' : 'quota.resetUncertain' : 'quota.resetPrepared')}</p>
      <code>{operation.id}</code>
      <p>{t('quota.resetLocked')}</p>
      <div className="button-row">
        <button type="button" className="secondary" disabled={busy} onClick={() => void inspect(false)}>{t('quota.resetCheckStatus')}</button>
        {dispatchState && <button type="button" className="secondary" disabled={busy} onClick={() => void inspect(true)}>{t('quota.resetReconcile')}</button>}
      </div>
      {operation.last_reconciled_at !== null && <p>{t('quota.resetReconciled', { time: new Date(operation.last_reconciled_at).toLocaleString(locale), available: operation.reconciled_available_credits ?? '—', applicable: operation.reconciled_applicable_credits ?? '—' })}</p>}
    </div>}
  </div>;
}
