import { useEffect, useRef, useState } from 'react';
import { api, ApiError } from '../api';
import { UPSTREAM_QUOTA_READ_TIMEOUT_MILLIS, upstreamQuotaPath, type UpstreamQuotaSnapshot } from './upstreamQuota';

export interface QuotaReadAccount { id: string; credential_generation: number; tenant_external_id?: string | null; status: string }
export interface QuotaReadState { generation: number; snapshot?: UpstreamQuotaSnapshot; busy: boolean; queued?: boolean; refreshFailed: boolean; error?: 'quota.readFailed' | 'quota.errorPermission' }

// A quota read can open multiple supplier connections through an account-bound
// SOCKS proxy. Keep the list action serial so one slow proxy cannot make the
// other rows compete for control-plane permits or saturate shared proxy nodes.
const BULK_QUOTA_READ_CONCURRENCY = 1;

/** One shared read owner for list, detail and batch actions; never calls reset or OAuth. */
export function useUpstreamQuotaReads(token: string, tenant: string, accounts: QuotaReadAccount[]) {
  const scope = `${token}\0${tenant}`;
  const current = useRef({ scope, accounts }); current.current = { scope, accounts };
  const requests = useRef(new Map<string, { controller: AbortController; promise: Promise<void> }>());
  const batch = useRef<object | undefined>(undefined);
  const [entries, setEntries] = useState<Record<string, QuotaReadState>>({});
  const [entryScope, setEntryScope] = useState(scope);
  const [progress, setProgress] = useState<{ done: number; total: number; busy: boolean }>();
  useEffect(() => {
    setEntries({}); setEntryScope(scope); setProgress(undefined); batch.current = undefined;
    return () => { for (const request of requests.current.values()) request.controller.abort(); requests.current.clear(); };
  }, [scope]);

  function read(account: QuotaReadAccount): Promise<void> {
    const accountTenant = account.tenant_external_id ?? tenant;
    const generation = account.credential_generation;
    const owns = () => current.current.scope === scope && current.current.accounts.some(value => value.id === account.id && value.credential_generation === generation && value.status === account.status && (value.tenant_external_id ?? tenant) === accountTenant);
    if (!token || !accountTenant || !owns()) return Promise.resolve();
    const key = `${scope}\0${accountTenant}\0${account.id}\0${generation}`;
    const existing = requests.current.get(key); if (existing) return existing.promise;
    const controller = new AbortController();
    setEntries(previous => ({ ...previous, [account.id]: { generation, snapshot: previous[account.id]?.generation === generation ? previous[account.id].snapshot : undefined, busy: true, refreshFailed: false } }));
    const promise = (async () => {
      try {
        const snapshot = await api<UpstreamQuotaSnapshot>(upstreamQuotaPath(account.id, accountTenant), token, { signal: AbortSignal.any([controller.signal, AbortSignal.timeout(UPSTREAM_QUOTA_READ_TIMEOUT_MILLIS)]) });
        if (!owns() || controller.signal.aborted) return;
        if (snapshot.contract_version !== 'upstream_quota_v1' || snapshot.upstream_account_id !== account.id || snapshot.tenant_external_id !== accountTenant) throw new Error('Quota scope mismatch');
        setEntries(previous => owns() ? { ...previous, [account.id]: { generation, snapshot, busy: false, refreshFailed: snapshot.status === 'error' || Boolean(snapshot.error_code) } } : previous);
      } catch (reason) {
        if (!owns() || controller.signal.aborted) return;
        const error = reason instanceof ApiError && [401, 403].includes(reason.status) ? 'quota.errorPermission' : 'quota.readFailed';
        setEntries(previous => owns() ? { ...previous, [account.id]: { generation, snapshot: previous[account.id]?.generation === generation ? previous[account.id].snapshot : undefined, busy: false, refreshFailed: true, error } } : previous);
      } finally { if (requests.current.get(key)?.controller === controller) requests.current.delete(key); }
    })();
    requests.current.set(key, { controller, promise });
    return promise;
  }

  async function readAll() {
    if (batch.current || requests.current.size) return;
    const selected = accounts.filter(account => account.status === 'active' && Boolean(account.tenant_external_id ?? tenant));
    if (!token || !selected.length) return;
    const run = {}; batch.current = run;
    let next = 0; let done = 0;
    setEntries(previous => {
      const queued = { ...previous };
      for (const account of selected) queued[account.id] = { generation: account.credential_generation, snapshot: previous[account.id]?.generation === account.credential_generation ? previous[account.id].snapshot : undefined, busy: false, queued: true, refreshFailed: false };
      return queued;
    });
    setProgress({ done, total: selected.length, busy: true });
    const owns = () => batch.current === run && current.current.scope === scope;
    await Promise.all(Array.from({ length: Math.min(BULK_QUOTA_READ_CONCURRENCY, selected.length) }, async () => {
      while (owns() && next < selected.length) {
        const account = selected[next++];
        await read(account); done++;
        if (owns()) setProgress({ done, total: selected.length, busy: true });
      }
    }));
    if (owns()) {
      batch.current = undefined;
      setEntries(previous => {
        const settled = { ...previous };
        for (const account of selected) {
          const entry = settled[account.id];
          if (entry?.generation === account.credential_generation && entry.queued) settled[account.id] = { ...entry, queued: false };
        }
        return settled;
      });
      setProgress({ done, total: selected.length, busy: false });
    }
  }
  const visibleEntries: Record<string, QuotaReadState> = entryScope === scope ? entries : {};
  return { entries: visibleEntries, read: (account: QuotaReadAccount) => batch.current ? Promise.resolve() : read(account), readAll, progress: entryScope === scope ? progress : undefined };
}
