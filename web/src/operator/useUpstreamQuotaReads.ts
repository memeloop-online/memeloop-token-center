import { useEffect, useRef, useState } from 'react';
import { api, ApiError } from '../api';
import { quotaEffectiveSnapshot, quotaRefreshDiagnostic, UPSTREAM_QUOTA_BATCH_TIMEOUT_MILLIS, UPSTREAM_QUOTA_READ_TIMEOUT_MILLIS, upstreamQuotaBatchPath, upstreamQuotaPath, type UpstreamQuotaBatchResponse, type UpstreamQuotaDiagnostic, type UpstreamQuotaReadTrigger, type UpstreamQuotaSnapshot } from './upstreamQuota';

export interface QuotaReadAccount { id: string; credential_generation: number; tenant_external_id?: string | null; status: string }
export interface QuotaReadState { generation: number; snapshot?: UpstreamQuotaSnapshot; diagnostic?: UpstreamQuotaDiagnostic; busy: boolean; queued?: boolean; refreshFailed: boolean; error?: 'quota.readFailed' | 'quota.errorPermission' }

function refreshFailed(snapshot: UpstreamQuotaSnapshot, diagnostic?: UpstreamQuotaDiagnostic) {
  return snapshot.status === 'error' || Boolean(snapshot.error_code) || Boolean(diagnostic?.error_code);
}

function diagnosticForApiFailure(reason: unknown): UpstreamQuotaDiagnostic {
  const status = reason instanceof ApiError ? reason.status : undefined;
  const code = status === 401 || status === 403 ? 'quota_not_authorized'
    : status === 408 || status === 504 ? 'quota_timeout'
      : status === 429 ? 'quota_rate_limited' : 'quota_read_failed';
  return { error_code: code, attempts: [], cache_hit: false };
}

/** One shared read owner for list, detail and batch actions; never calls reset or OAuth. */
export function useUpstreamQuotaReads(token: string, tenant: string, accounts: QuotaReadAccount[]) {
  const scope = `${token}\0${tenant}`;
  const current = useRef({ scope, accounts }); current.current = { scope, accounts };
  const requests = useRef(new Map<string, { controller: AbortController; promise: Promise<void> }>());
  const batch = useRef<{ controller: AbortController } | undefined>(undefined);
  const [entries, setEntries] = useState<Record<string, QuotaReadState>>({});
  const [entryScope, setEntryScope] = useState(scope);
  const [progress, setProgress] = useState<{ done: number; total: number; busy: boolean }>();
  useEffect(() => {
    setEntries({}); setEntryScope(scope); setProgress(undefined); batch.current?.controller.abort(); batch.current = undefined;
    return () => { batch.current?.controller.abort(); batch.current = undefined; for (const request of requests.current.values()) request.controller.abort(); requests.current.clear(); };
  }, [scope]);

  function read(account: QuotaReadAccount, trigger: UpstreamQuotaReadTrigger): Promise<void> {
    const accountTenant = account.tenant_external_id ?? tenant;
    const generation = account.credential_generation;
    const ownsIdentity = () => current.current.scope === scope && current.current.accounts.some(value => value.id === account.id && value.credential_generation === generation && (value.tenant_external_id ?? tenant) === accountTenant);
    const isEligible = () => current.current.accounts.some(value => value.id === account.id && value.credential_generation === generation && value.status === 'active' && (value.tenant_external_id ?? tenant) === accountTenant);
    const clearPending = () => setEntries(previous => {
      const entry = previous[account.id];
      return entry?.generation === generation ? { ...previous, [account.id]: { ...entry, busy: false, queued: false } } : previous;
    });
    if (!token || !accountTenant || !ownsIdentity() || !isEligible()) return Promise.resolve();
    const key = `${scope}\0${accountTenant}\0${account.id}\0${generation}`;
    const existing = requests.current.get(key); if (existing) return existing.promise;
    const controller = new AbortController();
    setEntries(previous => ({ ...previous, [account.id]: { generation, snapshot: previous[account.id]?.generation === generation ? previous[account.id].snapshot : undefined, diagnostic: previous[account.id]?.generation === generation ? previous[account.id].diagnostic : undefined, busy: true, refreshFailed: false } }));
    const promise = (async () => {
      try {
        const snapshot = await api<UpstreamQuotaSnapshot>(upstreamQuotaPath(account.id, accountTenant, { fresh: true, trigger }), token, { signal: AbortSignal.any([controller.signal, AbortSignal.timeout(UPSTREAM_QUOTA_READ_TIMEOUT_MILLIS)]) });
        if (!ownsIdentity() || controller.signal.aborted) return;
        if (!isEligible()) { clearPending(); return; }
        if (snapshot.contract_version !== 'upstream_quota_v1' || snapshot.upstream_account_id !== account.id || snapshot.tenant_external_id !== accountTenant) throw new Error('Quota scope mismatch');
        setEntries(previous => {
          if (!ownsIdentity() || !isEligible()) return previous;
          const previousEntry = previous[account.id]?.generation === generation ? previous[account.id] : undefined;
          const diagnostic = quotaRefreshDiagnostic(snapshot);
          return {
            ...previous,
            [account.id]: {
              generation,
              snapshot: quotaEffectiveSnapshot(previousEntry?.snapshot, snapshot),
              diagnostic,
              busy: false,
              refreshFailed: refreshFailed(snapshot, diagnostic),
            },
          };
        });
      } catch (reason) {
        if (!ownsIdentity() || controller.signal.aborted) return;
        if (!isEligible()) { clearPending(); return; }
        const error = reason instanceof ApiError && [401, 403].includes(reason.status) ? 'quota.errorPermission' : 'quota.readFailed';
        const diagnostic = diagnosticForApiFailure(reason);
        setEntries(previous => ownsIdentity() && isEligible() ? { ...previous, [account.id]: { generation, snapshot: previous[account.id]?.generation === generation ? previous[account.id].snapshot : undefined, diagnostic, busy: false, refreshFailed: true, error } } : previous);
      } finally { if (requests.current.get(key)?.controller === controller) requests.current.delete(key); }
    })();
    requests.current.set(key, { controller, promise });
    return promise;
  }

  async function readAll() {
    if (batch.current || requests.current.size) return;
    const selected = accounts.filter(account => account.status === 'active' && Boolean(account.tenant_external_id ?? tenant));
    if (!token || !selected.length) return;
    const run = { controller: new AbortController() }; batch.current = run;
    setEntries(previous => {
      const queued = { ...previous };
      for (const account of selected) queued[account.id] = { generation: account.credential_generation, snapshot: previous[account.id]?.generation === account.credential_generation ? previous[account.id].snapshot : undefined, diagnostic: previous[account.id]?.generation === account.credential_generation ? previous[account.id].diagnostic : undefined, busy: false, queued: true, refreshFailed: false };
      return queued;
    });
    setProgress({ done: 0, total: selected.length, busy: true });
    const owns = () => batch.current === run && current.current.scope === scope;
    try {
      const response = await api<UpstreamQuotaBatchResponse>(upstreamQuotaBatchPath(), token, {
        method: 'POST',
        body: JSON.stringify({ account_ids: selected.map(account => account.id), fresh: true, trigger: 'bulk' }),
        signal: AbortSignal.any([run.controller.signal, AbortSignal.timeout(UPSTREAM_QUOTA_BATCH_TIMEOUT_MILLIS)]),
      });
      if (!owns()) return;
      if (response.contract_version !== 'upstream_quota_batch_v1') throw new Error('Quota batch contract mismatch');
      const selectedIds = new Set(selected.map(account => account.id));
      const resultIds = new Set(response.results.map(result => result.upstream_account_id));
      if (response.results.length !== selected.length || resultIds.size !== selected.length || [...resultIds].some(id => !selectedIds.has(id))) throw new Error('Quota batch identity mismatch');
      const results = new Map(response.results.map(result => [result.upstream_account_id, result]));
      setEntries(previous => {
        const settled = { ...previous };
        for (const account of selected) {
          const accountTenant = account.tenant_external_id ?? tenant;
          const currentAccount = current.current.accounts.find(value => value.id === account.id);
          const eligible = currentAccount?.credential_generation === account.credential_generation && currentAccount.status === 'active' && (currentAccount.tenant_external_id ?? tenant) === accountTenant;
          if (!eligible) {
            const entry = settled[account.id];
            if (entry?.generation === account.credential_generation) settled[account.id] = { ...entry, busy: false, queued: false };
            continue;
          }
          const result = results.get(account.id);
          if (result?.status === 'success' && result.snapshot.contract_version === 'upstream_quota_v1' && result.snapshot.upstream_account_id === account.id && result.snapshot.tenant_external_id === accountTenant) {
            const diagnostic = quotaRefreshDiagnostic(result.snapshot);
            const previousEntry = previous[account.id]?.generation === account.credential_generation ? previous[account.id] : undefined;
            settled[account.id] = {
              generation: account.credential_generation,
              snapshot: quotaEffectiveSnapshot(previousEntry?.snapshot, result.snapshot),
              diagnostic,
              busy: false,
              queued: false,
              refreshFailed: refreshFailed(result.snapshot, diagnostic),
            };
          } else {
            const diagnostic = result?.status === 'error'
              ? quotaRefreshDiagnostic(undefined, result.error.code, result.diagnostic)
              : { error_code: 'quota_batch_timeout', attempts: [], cache_hit: false };
            const previousEntry = previous[account.id]?.generation === account.credential_generation ? previous[account.id] : undefined;
            settled[account.id] = { generation: account.credential_generation, snapshot: previousEntry?.snapshot, diagnostic, busy: false, queued: false, refreshFailed: true, error: diagnostic.error_code === 'quota_not_authorized' ? 'quota.errorPermission' : 'quota.readFailed' };
          }
        }
        return settled;
      });
    } catch (reason) {
      if (!owns() || run.controller.signal.aborted) return;
      const error = reason instanceof ApiError && [401, 403].includes(reason.status) ? 'quota.errorPermission' : 'quota.readFailed';
      const diagnostic = diagnosticForApiFailure(reason);
      setEntries(previous => {
        const settled = { ...previous };
        for (const account of selected) {
          const entry = settled[account.id];
          if (entry?.generation === account.credential_generation) settled[account.id] = { ...entry, busy: false, queued: false, refreshFailed: true, diagnostic, error };
        }
        return settled;
      });
    } finally {
      if (!owns()) return;
      batch.current = undefined;
      setProgress({ done: selected.length, total: selected.length, busy: false });
    }
  }
  const visibleEntries: Record<string, QuotaReadState> = entryScope === scope ? entries : {};
  return { entries: visibleEntries, read: (account: QuotaReadAccount) => batch.current ? Promise.resolve() : read(account, 'manual'), readAll, progress: entryScope === scope ? progress : undefined };
}
