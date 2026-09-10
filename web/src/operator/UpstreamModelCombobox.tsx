import { useEffect, useMemo, useRef, useState } from 'react';
import { api } from '../api';
import { formatNumber } from '../format';
import { useI18n } from '../i18n';
import { ModelPicker, type ModelPickerOption } from '../ModelPicker';
import type { UpstreamAccount } from '../types';

interface CatalogModel {
  id: string;
  protocol: string;
  supported_account_count: number;
  eligible_account_count: number;
  complete_coverage: boolean;
  context_window?: number;
  reservation_token_bound?: number;
}

interface AggregateCatalog {
  data: CatalogModel[];
  eligible_account_count: number;
  unknown_account_count: number;
  stale_account_count: number;
}

interface AccountCatalog {
  status: 'unknown' | 'syncing' | 'ready' | 'stale' | 'failed' | string;
  error_code?: string;
  models?: Array<{ id: string; protocol: string }>;
}

function delay(milliseconds: number) {
  return new Promise((resolve) => window.setTimeout(resolve, milliseconds));
}

interface Props {
  token: string;
  tenant: string;
  accountIds: string[];
  includedProviderGroupIds: string[];
  excludedProviderGroupIds: string[];
  syncAccountIds: string[];
  protocol: string;
  value: string;
  onChange: (value: string) => void;
  customModelConfirmed: boolean;
  onValidityChange: (valid: boolean, allowCustom: boolean) => void;
  upstreams?: UpstreamAccount[];
}

export function UpstreamModelCombobox({ token, tenant, accountIds, includedProviderGroupIds, excludedProviderGroupIds, syncAccountIds, protocol, value, onChange, customModelConfirmed, onValidityChange, upstreams = [] }: Props) {
  const { locale, t } = useI18n();
  const [catalog, setCatalog] = useState<AggregateCatalog>();
  const [browseCatalog, setBrowseCatalog] = useState<AggregateCatalog>();
  const [searchQuery, setSearchQuery] = useState('');
  const [browsing, setBrowsing] = useState(false);
  const [browseError, setBrowseError] = useState('');
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');
  const [syncMessage, setSyncMessage] = useState('');
  const [open, setOpen] = useState(false);
  const [accountCatalogs, setAccountCatalogs] = useState<Map<string, AccountCatalog>>(new Map());
  const [customConfirmed, setCustomConfirmed] = useState(customModelConfirmed);
  const [partialConfirmed, setPartialConfirmed] = useState(false);
  const [refreshVersion, setRefreshVersion] = useState(0);
  const validityCallback = useRef(onValidityChange);
  useEffect(() => { validityCallback.current = onValidityChange; }, [onValidityChange]);
  const sourceKey = `${accountIds.join(',')}|${includedProviderGroupIds.join(',')}|${excludedProviderGroupIds.join(',')}|${syncAccountIds.join(',')}|${protocol}`;
  const hasCandidates = accountIds.length > 0 || includedProviderGroupIds.length > 0;
  const customAllowed = accountIds.length > 0 && includedProviderGroupIds.length === 0 && excludedProviderGroupIds.length === 0;
  // A provider/account query is local provenance filtering, not a model-ID q.
  const sourceSearch = searchQuery.trim().toLowerCase();
  const matchesSource = sourceSearch && upstreams.some(account => syncAccountIds.includes(account.id)
    && [account.driver, account.name, account.id].some(text => text.toLowerCase().includes(sourceSearch)));
  const browseModelQuery = matchesSource ? '' : searchQuery.trim();

  useEffect(() => {
    setBrowseCatalog(undefined); setBrowseError('');
    if (!open || !token || !tenant || !hasCandidates) { setBrowsing(false); return; }
    const controller = new AbortController();
    setBrowsing(true);
    const timeout = window.setTimeout(async () => {
      const query = new URLSearchParams({ tenant_external_id: tenant, limit: '100' });
      if (accountIds.length) query.set('account_ids', accountIds.join(','));
      if (includedProviderGroupIds.length) query.set('include_provider_group_ids', includedProviderGroupIds.join(','));
      if (excludedProviderGroupIds.length) query.set('exclude_provider_group_ids', excludedProviderGroupIds.join(','));
      if (browseModelQuery) query.set('q', browseModelQuery);
      try {
        const result = await api<AggregateCatalog>(`/internal/v1/upstream-models?${query}`, token, { signal: controller.signal });
        if (!controller.signal.aborted) setBrowseCatalog(result);
      } catch (reason) {
        if (!controller.signal.aborted) setBrowseError(reason instanceof Error ? reason.message : t('routes.catalogFailed'));
      } finally { if (!controller.signal.aborted) setBrowsing(false); }
    }, 250);
    return () => { window.clearTimeout(timeout); controller.abort(); };
  }, [open, token, tenant, sourceKey, browseModelQuery, refreshVersion]);

  useEffect(() => {
    setCustomConfirmed(customModelConfirmed);
    setPartialConfirmed(false);
    // A catalog is scoped to the exact candidate set, protocol and query.
    // Keeping the previous result visible during the debounce can make a model
    // look selected for the newly chosen upstream and suppress the explicit
    // custom-model confirmation until the next request settles. Clear it
    // synchronously so neither validity nor UI can borrow stale coverage.
    setCatalog(undefined);
    if (!token || !tenant || !hasCandidates) { setCatalog(undefined); setError(''); setLoading(false); return; }
    const controller = new AbortController();
    const timeout = window.setTimeout(async () => {
      const query = new URLSearchParams({ tenant_external_id: tenant, limit: '100' });
      if (accountIds.length) query.set('account_ids', accountIds.join(','));
      if (includedProviderGroupIds.length) query.set('include_provider_group_ids', includedProviderGroupIds.join(','));
      if (excludedProviderGroupIds.length) query.set('exclude_provider_group_ids', excludedProviderGroupIds.join(','));
      if (value.trim()) query.set('q', value.trim());
      setLoading(true); setError('');
      try { setCatalog(await api<AggregateCatalog>(`/internal/v1/upstream-models?${query}`, token, { signal: controller.signal })); }
      catch (reason) { if (!controller.signal.aborted) { setCatalog(undefined); setError(reason instanceof Error ? reason.message : t('routes.catalogFailed')); } }
      finally { if (!controller.signal.aborted) setLoading(false); }
    }, 250);
    return () => { window.clearTimeout(timeout); controller.abort(); };
  }, [token, tenant, sourceKey, value, refreshVersion]);

  useEffect(() => {
    let current = true;
    const controller = new AbortController();
    setAccountCatalogs(new Map());
    if (!open || !token || !tenant) return;
    const ids = [...new Set(syncAccountIds)];
    const catalogScope = new URLSearchParams({ tenant_external_id: tenant, limit: '200' });
    if (browseModelQuery) catalogScope.set('q', browseModelQuery);
    let cursor = 0;
    const read = async () => {
      while (current && cursor < ids.length) {
        const accountId = ids[cursor++];
        try {
          const next = await api<AccountCatalog>(`/internal/v1/upstreams/${encodeURIComponent(accountId)}/models?${catalogScope}`, token, { signal: controller.signal });
          if (current) setAccountCatalogs((catalogs) => new Map(catalogs).set(accountId, next));
        } catch {
          // Missing account provenance stays explicitly unknown; aggregate
          // coverage and custom/partial-model validation are never inferred.
        }
      }
    };
    const timeout = window.setTimeout(() => {
      void Promise.all(Array.from({ length: Math.min(4, ids.length) }, read));
    }, 250);
    return () => { current = false; controller.abort(); window.clearTimeout(timeout); };
  }, [open, token, tenant, sourceKey, browseModelQuery, refreshVersion]);

  const options = useMemo(() => ((open ? browseCatalog : catalog)?.data ?? []).filter((model) => model.protocol === protocol || model.protocol === 'any'), [open, browseCatalog, catalog, protocol]);
  const selected = catalog?.data.find((model) => model.id === value && (model.protocol === protocol || model.protocol === 'any'));
  const catalogFresh = Boolean(catalog && catalog.unknown_account_count === 0 && catalog.stale_account_count === 0);
  const selectedValid = Boolean(selected && catalogFresh && (selected.complete_coverage || partialConfirmed));
  // A model returned by a stale or incomplete catalog is not verified. For
  // an exact account selection, keep the explicit custom-model escape hatch
  // available instead of leaving the form in a state with neither a usable
  // confirmation nor a valid submit button while synchronization settles.
  const needsCustomConfirmation = Boolean(value.trim() && (!selected || !catalogFresh));
  const allowCustom = Boolean(needsCustomConfirmation && customAllowed && customConfirmed);
  const valid = Boolean(selectedValid || allowCustom);
  useEffect(() => validityCallback.current(valid, allowCustom), [valid, allowCustom]);

  const choose = (model: CatalogModel) => {
    onChange(model.id); setCustomConfirmed(false); setPartialConfirmed(false);
  };
  const sync = async () => {
    if (syncAccountIds.length === 0) return;
    setLoading(true); setError(''); setSyncMessage('');
    try {
      const query = new URLSearchParams({ tenant_external_id: tenant });
      await Promise.all(syncAccountIds.map(async (accountId) => {
        let accountCatalog = await api<AccountCatalog>(`/internal/v1/upstreams/${accountId}/models/sync?${query}`, token, { method: 'POST' });
        for (let attempt = 0; accountCatalog.status === 'syncing' && attempt < 40; attempt += 1) {
          await delay(250);
          accountCatalog = await api<AccountCatalog>(`/internal/v1/upstreams/${accountId}/models?${query}`, token);
        }
        if (accountCatalog.status !== 'ready') {
          throw new Error(accountCatalog.error_code || t('routes.syncModelsFailed'));
        }
      }));
      setSyncMessage(t('routes.syncModelsComplete', { count: formatNumber(syncAccountIds.length, locale) }));
      setRefreshVersion((current) => current + 1);
    } catch (reason) { setError(reason instanceof Error ? reason.message : t('routes.catalogFailed')); }
    finally { setLoading(false); }
  };
  const groupedOptions: ModelPickerOption[] = options.flatMap((model) => {
    const accounts = upstreams.filter((account) => syncAccountIds.includes(account.id) && accountCatalogs.get(account.id)?.models?.some((item) => item.id === model.id && (item.protocol === protocol || item.protocol === 'any')));
    return (accounts.length ? accounts : [undefined]).map((account) => ({
      key: `${account?.id ?? 'unknown'}:${model.protocol}:${model.id}`, value: model.id, label: model.id,
      provider: account?.driver || t('modelPicker.unknown'), upstream: account?.name || t('modelPicker.unknown'),
      description: [
        model.complete_coverage ? t('routes.fullCoverage') : t('routes.partialCoverage', { supported: formatNumber(model.supported_account_count, locale), eligible: formatNumber(model.eligible_account_count, locale) }),
        model.context_window ? t('routes.contextWindow', { count: formatNumber(model.context_window, locale) }) : '',
        model.reservation_token_bound ? t('routes.reservationBound', { count: formatNumber(model.reservation_token_bound, locale) }) : '',
      ].filter(Boolean).join(' · '),
    }));
  });

  return <div className="model-combobox">
    <ModelPicker label={t('routes.upstreamModel')} editable searchableEditable invalid={!valid && Boolean(value.trim())} value={value} options={groupedOptions}
      loading={open ? browsing : loading} error={open ? browseError : error} onQueryChange={setSearchQuery} onOpen={() => setOpen(true)} onClose={() => setOpen(false)} onChange={(next) => {
      const option = options.find((model) => model.id === next);
      if (option) choose(option);
      else { onChange(next); setCustomConfirmed(false); setPartialConfirmed(false); }
    }} />
    {open && <small className="field-hint">{t('routing.catalogSearchHint')}</small>}
    <div className="catalog-status"><small className="field-hint">{loading ? t('routes.catalogLoading') : error || syncMessage || (catalog ? t('routes.catalogCoverage', { eligible: formatNumber(catalog.eligible_account_count, locale), unknown: formatNumber(catalog.unknown_account_count, locale), stale: formatNumber(catalog.stale_account_count, locale) }) : t('routes.selectCandidatesFirst'))}</small>{syncAccountIds.length > 0 && <button type="button" className="secondary" disabled={loading} onClick={() => void sync()}>{t('routes.syncModels')}</button>}</div>
    {selected && !selected.complete_coverage && <div className="custom-model-confirm"><label><input type="checkbox" checked={partialConfirmed} onChange={(event) => setPartialConfirmed(event.target.checked)} />{t('routes.confirmPartialCoverage', { supported: formatNumber(selected.supported_account_count, locale), eligible: formatNumber(selected.eligible_account_count, locale) })}</label></div>}
    {selected && catalog && (catalog.unknown_account_count > 0 || catalog.stale_account_count > 0) && <div className="notice warning compact">{t('routes.catalogNotReady')}</div>}
    {needsCustomConfirmation && <div className={`custom-model-confirm${customAllowed ? '' : ' disabled'}`}>
      {customAllowed ? <label><input type="checkbox" checked={customConfirmed} onChange={(event) => setCustomConfirmed(event.target.checked)} />{t('routes.confirmCustomModel', { model: value.trim() })}</label> : <span>{t('routes.customUnavailableForGroups')}</span>}
    </div>}
  </div>;
}
