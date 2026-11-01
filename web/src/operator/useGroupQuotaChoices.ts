import { useEffect, useState } from 'react';
import { api, ApiError } from '../api';
import { useI18n } from '../i18n';
import type { ResourceChoiceOption } from '../resourceChoice';
import type { ComboboxOption } from './MultiCombobox';
import { quotaWindowPresentation, upstreamQuotaPath, UPSTREAM_QUOTA_READ_TIMEOUT_MILLIS, type UpstreamQuotaSnapshot } from './upstreamQuota';

type WindowSnapshot = Pick<UpstreamQuotaSnapshot, 'provider' | 'upstream_account_id'> & { windows: Pick<UpstreamQuotaSnapshot['windows'][number], 'id' | 'label' | 'period_seconds'>[] };

export function quotaChoiceLabel(provider: string, window: WindowSnapshot['windows'][number], t: (key: string, variables?: Record<string, string | number>) => string, providerName?: string) {
  const presentation = quotaWindowPresentation(provider, window);
  const scope = presentation.scopeKey === 'quota.scopeCodex' ? 'Codex' : presentation.supplierLabel ?? (presentation.scopeKey === 'quota.scopeSupplier' ? providerName ?? t(presentation.scopeKey) : t(presentation.scopeKey));
  const name = presentation.qualifier ? `${scope} · ${presentation.qualifier}` : scope;
  const periods: Record<string, string> = { 'quota.periodFiveHour': 'resourceChoice.fiveHour', 'quota.periodDaily': 'resourceChoice.daily', 'quota.periodWeekly': 'resourceChoice.weekly', 'quota.periodMonthly': 'resourceChoice.monthly', 'quota.periodAnnual': 'resourceChoice.annual' };
  return t('quota.windowLabel', { scope: name, period: t(periods[presentation.periodKey] ?? presentation.periodKey) });
}

/** Persisted window names are shown immediately; missing metadata is discovered in the background. */
export function useGroupQuotaChoices(enabled: boolean, discover: boolean, accounts: ComboboxOption[], token: string, tenant: string) {
  const { t } = useI18n();
  const accountKey = JSON.stringify([...new Set(accounts.map(account => account.value))].sort());
  const scope = JSON.stringify([token, tenant, accountKey]);
  const [result, setResult] = useState<{ scope: string; snapshots: WindowSnapshot[]; busy: boolean; error: string }>({ scope: '', snapshots: [], busy: false, error: '' });
  const [reload, setReload] = useState(0);
  useEffect(() => {
    if (!enabled || !token || !tenant) return;
    const ids = JSON.parse(accountKey) as string[];
    const controller = new AbortController();
    setResult(current => ({ scope, snapshots: current.scope === scope ? current.snapshots : [], busy: ids.length > 0, error: '' }));
    if (!ids.length) return;
    const batches = Array.from({ length: Math.ceil(ids.length / 100) }, (_, index) => ids.slice(index * 100, index * 100 + 100));
    void (async () => {
      let snapshots: WindowSnapshot[] = result.scope === scope ? result.snapshots : [];
      const missing: string[] = [];
      try {
        for (const account_ids of batches) {
          const metadata = await api<{ results: WindowSnapshot[] }>('/internal/v1/upstreams/quota/windows', token, {
            method: 'POST', body: JSON.stringify({ account_ids, tenant_external_id: tenant }),
            signal: AbortSignal.any([controller.signal, AbortSignal.timeout(10_000)]),
          });
          if (controller.signal.aborted) return;
          const known = metadata.results.filter(row => account_ids.includes(row.upstream_account_id));
          snapshots = [...snapshots.filter(row => !account_ids.includes(row.upstream_account_id)), ...known];
          missing.push(...account_ids.filter(id => !known.some(row => row.upstream_account_id === id && row.windows.length)));
          setResult({ scope, snapshots, busy: false, error: '' });
        }
      } catch (error) {
        if (controller.signal.aborted) return;
        setResult({ scope, snapshots, busy: false, error: error instanceof ApiError && [401, 403].includes(error.status) ? 'quota.errorPermission' : 'resourceChoice.loadFailed' });
        return;
      }
      if (!discover) return;
      let next = 0;
      let pending = missing.length;
      let failed = false;
      if (pending) setResult({ scope, snapshots, busy: true, error: '' });
      await Promise.all(Array.from({ length: Math.min(3, missing.length) }, async () => {
        while (next < missing.length && !controller.signal.aborted) {
          const accountId = missing[next++];
          try {
            const snapshot = await api<UpstreamQuotaSnapshot>(upstreamQuotaPath(accountId, tenant, { trigger: 'bulk' }), token, {
              signal: AbortSignal.any([controller.signal, AbortSignal.timeout(UPSTREAM_QUOTA_READ_TIMEOUT_MILLIS)]),
            });
            if (controller.signal.aborted) return;
            if (snapshot.upstream_account_id === accountId) snapshots = [...snapshots.filter(row => row.upstream_account_id !== accountId), snapshot];
            else failed = true;
          } catch {
            if (controller.signal.aborted) return;
            failed = true;
          }
          pending--;
          setResult({ scope, snapshots, busy: pending > 0, error: failed ? 'resourceChoice.partial' : '' });
        }
      }));
    })();
    return () => controller.abort();
  }, [enabled, discover, token, tenant, accountKey, reload]);
  const visible = result.scope === scope ? result : { snapshots: [], busy: enabled && Boolean(accounts.length), error: '' };
  const choices = new Map<string, ResourceChoiceOption>();
  for (const snapshot of visible.snapshots) for (const window of snapshot.windows) {
    if (!window.id || !snapshot.provider) continue;
    const value = JSON.stringify([snapshot.provider, window.id]);
    choices.set(value, { value, label: quotaChoiceLabel(snapshot.provider, window, t, accounts.find(account => account.value === snapshot.upstream_account_id)?.description),
      details: `${snapshot.provider} · ${window.id}`, data: { provider: snapshot.provider, window_id: window.id } });
  }
  return { options: [...choices.values()], busy: visible.busy, error: visible.error ? t(visible.error) : '', retry: () => setReload(value => value + 1) };
}
