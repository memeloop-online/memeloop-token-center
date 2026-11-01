import { useEffect, useState } from 'react';
import { api, ApiError } from '../api';
import { useI18n } from '../i18n';
import type { ResourceChoiceOption } from '../resourceChoice';
import type { ComboboxOption } from './MultiCombobox';
import { quotaWindowPresentation, upstreamQuotaBatchPath, UPSTREAM_QUOTA_BATCH_TIMEOUT_MILLIS, type UpstreamQuotaBatchResponse, type UpstreamQuotaSnapshot } from './upstreamQuota';

/** Configuration discovery shares the existing account quota cache. */
export function useGroupQuotaChoices(enabled: boolean, accounts: ComboboxOption[], token: string, tenant: string) {
  const { t } = useI18n();
  const accountKey = JSON.stringify([...new Set(accounts.map(account => account.value))].sort());
  const scope = JSON.stringify([token, tenant, accountKey]);
  const [result, setResult] = useState<{ scope: string; snapshots: UpstreamQuotaSnapshot[]; busy: boolean; error: string }>({ scope: '', snapshots: [], busy: false, error: '' });
  const [reload, setReload] = useState(0);
  useEffect(() => {
    if (!enabled || !token || !tenant) return;
    const ids = JSON.parse(accountKey) as string[];
    const controller = new AbortController();
    setResult(current => ({ scope, snapshots: current.scope === scope ? current.snapshots : [], busy: ids.length > 0, error: '' }));
    if (!ids.length) return;
    const batches = Array.from({ length: Math.ceil(ids.length / 100) }, (_, index) => ids.slice(index * 100, index * 100 + 100));
    void (async () => {
      let snapshots: UpstreamQuotaSnapshot[] = [];
      let failed = false;
      for (const account_ids of batches) {
        try {
          const response = await api<UpstreamQuotaBatchResponse>(upstreamQuotaBatchPath(), token, {
            method: 'POST', body: JSON.stringify({ account_ids, fresh: false, trigger: 'bulk' }),
            signal: AbortSignal.any([controller.signal, AbortSignal.timeout(UPSTREAM_QUOTA_BATCH_TIMEOUT_MILLIS)]),
          });
          if (controller.signal.aborted) return;
          snapshots = [...snapshots, ...response.results.flatMap(row => row.status === 'success' && account_ids.includes(row.upstream_account_id) ? [row.snapshot] : [])];
          failed ||= response.results.some(row => row.status === 'error');
          setResult({ scope, snapshots, busy: true, error: '' });
        } catch (error) {
          if (controller.signal.aborted) return;
          failed = true;
          setResult({ scope, snapshots, busy: false, error: error instanceof ApiError && [401, 403].includes(error.status) ? 'quota.errorPermission' : 'resourceChoice.loadFailed' });
          return;
        }
      }
      setResult({ scope, snapshots, busy: false, error: failed ? 'resourceChoice.partial' : '' });
    })();
    return () => controller.abort();
  }, [enabled, token, tenant, accountKey, reload]);
  const visible = result.scope === scope ? result : { snapshots: [], busy: enabled && Boolean(accounts.length), error: '' };
  const choices = new Map<string, ResourceChoiceOption>();
  for (const snapshot of visible.snapshots) for (const window of snapshot.windows) {
    if (!window.id || !snapshot.provider) continue;
    const presentation = quotaWindowPresentation(snapshot.provider, window);
    const scopeName = presentation.scopeKey === 'quota.scopeCodex' ? 'Codex' : presentation.supplierLabel ?? (presentation.scopeKey === 'quota.scopeSupplier'
      ? accounts.find(account => account.value === snapshot.upstream_account_id)?.description ?? t(presentation.scopeKey) : t(presentation.scopeKey));
    const name = presentation.qualifier ? `${scopeName} · ${presentation.qualifier}` : scopeName;
    const value = JSON.stringify([snapshot.provider, window.id]);
    const periodLabels: Record<string, string> = { 'quota.periodFiveHour': 'resourceChoice.fiveHour', 'quota.periodDaily': 'resourceChoice.daily', 'quota.periodWeekly': 'resourceChoice.weekly', 'quota.periodMonthly': 'resourceChoice.monthly', 'quota.periodAnnual': 'resourceChoice.annual' };
    choices.set(value, { value, label: t('quota.windowLabel', { scope: name, period: t(periodLabels[presentation.periodKey] ?? presentation.periodKey) }),
      details: `${snapshot.provider} · ${window.id}`, data: { provider: snapshot.provider, window_id: window.id } });
  }
  return { options: [...choices.values()], busy: visible.busy, error: visible.error ? t(visible.error) : '', retry: () => setReload(value => value + 1) };
}
