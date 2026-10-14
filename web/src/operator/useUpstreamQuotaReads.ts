import { useEffect, useRef, useState } from 'react';
import { api, ApiError } from '../api';
import { quotaEffectiveSnapshot, quotaRefreshDiagnostic, UPSTREAM_QUOTA_READ_TIMEOUT_MILLIS, upstreamQuotaPath, type UpstreamQuotaDiagnostic, type UpstreamQuotaReadTrigger, type UpstreamQuotaSnapshot } from './upstreamQuota';

export interface QuotaReadAccount { id: string; credential_generation: number; tenant_external_id?: string | null; status: string }
export interface QuotaReadState { generation: number; snapshot?: UpstreamQuotaSnapshot; diagnostic?: UpstreamQuotaDiagnostic; busy: boolean; queued?: boolean; refreshFailed: boolean; error?: 'quota.readFailed' | 'quota.errorPermission' }

function refreshFailed(snapshot: UpstreamQuotaSnapshot, diagnostic?: UpstreamQuotaDiagnostic) {
  return snapshot.status === 'error' || Boolean(snapshot.error_code) || Boolean(diagnostic?.error_code);
}

function diagnosticForApiFailure(reason: unknown): UpstreamQuotaDiagnostic {
  const status = reason instanceof ApiError ? reason.status : undefined;
  const code = reason instanceof DOMException && reason.name === 'TimeoutError' ? 'quota_timeout'
    : status === 401 || status === 403 ? 'quota_not_authorized'
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
    setEntries(previous => ({ ...previous, [account.id]: { generation, snapshot: previous[account.id]?.generation === generation ? previous[account.id].snapshot : undefined, diagnostic: previous[account.id]?.generation === generation ? previous[account.id].diagnostic : undefined, busy: true, refreshFailed: previous[account.id]?.generation === generation ? previous[account.id].refreshFailed : false } }));
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
        setEntries(previous => {
          if (!ownsIdentity() || !isEligible()) return previous;
          const snapshot = previous[account.id]?.generation === generation ? previous[account.id].snapshot : undefined;
          return { ...previous, [account.id]: { generation, snapshot: snapshot ? { ...snapshot, stale: true, freshness: 'stale' } : undefined, diagnostic, busy: false, refreshFailed: true, error } };
        });
      } finally {
        if (requests.current.get(key)?.controller === controller) requests.current.delete(key);
        // A fenced or superseded read still releases its own pending entry so a
        // later auto-read is not blocked forever; newer generations keep their
        // own state and late results never publish.
        setEntries(previous => {
          if (current.current.scope !== scope) return previous;
          const entry = previous[account.id];
          return entry?.generation === generation && entry.busy ? { ...previous, [account.id]: { ...entry, busy: false, queued: false } } : previous;
        });
      }
    })();
    requests.current.set(key, { controller, promise });
    return promise;
  }

  async function readAll() {
    if (batch.current) return;
    const selected = accounts.filter(account => account.status === 'active' && Boolean(account.tenant_external_id ?? tenant));
    if (!token || !selected.length) return;
    const run = { controller: new AbortController() }; batch.current = run;
    setProgress({ done: 0, total: selected.length, busy: true });
    const owns = () => batch.current === run && current.current.scope === scope;
    let done = 0;
    try {
      await Promise.allSettled(selected.map(async account => {
        await read(account, 'bulk');
        if (!owns()) return;
        done += 1;
        setProgress({ done, total: selected.length, busy: done < selected.length });
      }));
    } finally {
      if (owns()) {
        batch.current = undefined;
        setProgress({ done: selected.length, total: selected.length, busy: false });
      }
    }
  }
  const visibleEntries: Record<string, QuotaReadState> = entryScope === scope ? entries : {};
  return { entries: visibleEntries, read: (account: QuotaReadAccount) => read(account, 'manual'), readAll, progress: entryScope === scope ? progress : undefined };
}
