import { useEffect, useState } from 'react';
import { api, ApiError } from '../api';
import { useI18n } from '../i18n';
import type { ResourceChoiceOption } from '../resourceChoice';
import type { ComboboxOption } from './MultiCombobox';
import { quotaWindowPresentation, upstreamQuotaBatchPath, UPSTREAM_QUOTA_BATCH_TIMEOUT_MILLIS, type UpstreamQuotaBatchResponse, type UpstreamQuotaSnapshot } from './upstreamQuota';

type WindowSnapshot = Pick<UpstreamQuotaSnapshot, 'provider' | 'upstream_account_id'> & { windows: Pick<UpstreamQuotaSnapshot['windows'][number], 'id' | 'label' | 'period_seconds'>[] };

export function quotaChoiceLabel(provider: string, window: WindowSnapshot['windows'][number], t: (key: string, variables?: Record<string, string | number>) => string, providerName?: string) {
  const presentation = quotaWindowPresentation(provider, window);
  const scope = presentation.scopeKey === 'quota.scopeCodex' ? 'Codex' : presentation.supplierLabel ?? (presentation.scopeKey === 'quota.scopeSupplier' ? providerName ?? t(presentation.scopeKey) : t(presentation.scopeKey));
  const name = presentation.qualifier ? `${scope} · ${presentation.qualifier}` : scope;
  const periods: Record<string, string> = { 'quota.periodFiveHour': 'resourceChoice.fiveHour', 'quota.periodDaily': 'resourceChoice.daily', 'quota.periodWeekly': 'resourceChoice.weekly', 'quota.periodMonthly': 'resourceChoice.monthly', 'quota.periodAnnual': 'resourceChoice.annual' };
  return t('quota.windowLabel', { scope: name, period: t(periods[presentation.periodKey] ?? presentation.periodKey) });
}

/** Persisted window names are shown immediately; missing metadata is discovered in the background. */
export function useGroupQuotaChoices(enabled: boolean, accounts: ComboboxOption[], token: string, tenant: string) {
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
      let snapshots: WindowSnapshot[] = [];
      const missing: string[] = [];
      try {
        for (const account_ids of batches) {
          const metadata = await api<{ results: WindowSnapshot[] }>('/internal/v1/upstreams/quota/windows', token, {
            method: 'POST', body: JSON.stringify({ account_ids, tenant_external_id: tenant }),
            signal: AbortSignal.any([controller.signal, AbortSignal.timeout(10_000)]),
          });
          if (controller.signal.aborted) return;
          const known = metadata.results.filter(row => account_ids.includes(row.upstream_account_id));
          snapshots = [...snapshots, ...known];
          missing.push(...account_ids.filter(id => !known.some(row => row.upstream_account_id === id && row.windows.length)));
          setResult({ scope, snapshots, busy: false, error: '' });
        }
      } catch (error) {
        if (controller.signal.aborted) return;
        setResult({ scope, snapshots, busy: false, error: error instanceof ApiError && [401, 403].includes(error.status) ? 'quota.errorPermission' : 'resourceChoice.loadFailed' });
        return;
      }
      for (let index = 0; index < missing.length; index += 100) {
        const account_ids = missing.slice(index, index + 100);
        setResult({ scope, snapshots, busy: true, error: '' });
        try {
          const response = await api<UpstreamQuotaBatchResponse>(upstreamQuotaBatchPath(), token, {
            method: 'POST', body: JSON.stringify({ account_ids, fresh: false, trigger: 'bulk' }),
            signal: AbortSignal.any([controller.signal, AbortSignal.timeout(UPSTREAM_QUOTA_BATCH_TIMEOUT_MILLIS)]),
          });
          if (controller.signal.aborted) return;
          const fresh = response.results.flatMap(row => row.status === 'success' && account_ids.includes(row.upstream_account_id) ? [row.snapshot] : []);
          snapshots = [...snapshots.filter(row => !fresh.some(value => value.upstream_account_id === row.upstream_account_id)), ...fresh];
          setResult({ scope, snapshots, busy: false, error: response.results.some(row => row.status === 'error') ? 'resourceChoice.partial' : '' });
        } catch (error) {
          if (controller.signal.aborted) return;
          setResult({ scope, snapshots, busy: false, error: error instanceof ApiError && [401, 403].includes(error.status) ? 'quota.errorPermission' : 'resourceChoice.loadFailed' });
          return;
        }
      }
    })();
    return () => controller.abort();
  }, [enabled, token, tenant, accountKey, reload]);
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
