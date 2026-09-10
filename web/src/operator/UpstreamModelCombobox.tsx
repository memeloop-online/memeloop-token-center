import { useEffect, useMemo, useRef, useState } from 'react';
import { api } from '../api';
import { formatNumber } from '../format';
import { useI18n } from '../i18n';
import { ModelPicker, type ModelPickerOption } from '../ModelPicker';
import type { ProviderType, UpstreamAccount } from '../types';

export function modelCatalogScopeKey(token: string, tenant: string, protocol: string, accountIds: string[], includedProviderGroupIds: string[], excludedProviderGroupIds: string[], syncAccountIds: string[], model: string) {
  return JSON.stringify([token, tenant, protocol, ...[accountIds, includedProviderGroupIds, excludedProviderGroupIds, syncAccountIds].map(ids => [...new Set(ids)].sort()), model.trim()]);
}

export interface CatalogValidity { scopeKey: string; valid: boolean; allowCustom: boolean }

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

function delay(milliseconds: number, signal: AbortSignal) {
  return new Promise<void>((resolve, reject) => {
    signal.throwIfAborted();
    const abort = () => { window.clearTimeout(timeout); reject(signal.reason); };
    const timeout = window.setTimeout(() => { signal.removeEventListener('abort', abort); resolve(); }, milliseconds);
    signal.addEventListener('abort', abort, { once: true });
  });
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
  onValidityChange: (validity: CatalogValidity) => void;
  upstreams?: UpstreamAccount[];
  providers?: ProviderType[];
}

export function UpstreamModelCombobox({ token, tenant, accountIds, includedProviderGroupIds, excludedProviderGroupIds, syncAccountIds, protocol, value, onChange, customModelConfirmed, onValidityChange, upstreams = [], providers = [] }: Props) {
  const { locale, t } = useI18n();
  const scopeKey = modelCatalogScopeKey(token, tenant, protocol, accountIds, includedProviderGroupIds, excludedProviderGroupIds, syncAccountIds, value);
  const [refreshVersion, setRefreshVersion] = useState(0);
  const catalogKey = JSON.stringify([scopeKey, refreshVersion]);
  const [catalogResult, setCatalog] = useState<{ scopeKey: string; data: AggregateCatalog }>();
  const catalog = catalogResult?.scopeKey === catalogKey ? catalogResult.data : undefined;
  const [browseResult, setBrowseCatalog] = useState<{ scopeKey: string; data: AggregateCatalog }>();
  const [searchQuery, setSearchQuery] = useState('');
  const [browsing, setBrowsing] = useState(false);
  const [browseError, setBrowseError] = useState('');
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');
  const [syncMessage, setSyncMessage] = useState('');
  const [syncing, setSyncing] = useState(false);
  const syncController = useRef<AbortController | undefined>(undefined);
  const latestScope = useRef(scopeKey);
  latestScope.current = scopeKey;
  const [open, setOpen] = useState(false);
  const [provenance, setProvenance] = useState<{ scopeKey: string; catalogs: Map<string, AccountCatalog> }>();
  const [provenanceLoading, setProvenanceLoading] = useState(false);
  const [customConfirmed, setCustomConfirmed] = useState(customModelConfirmed);
  const [partialConfirmed, setPartialConfirmed] = useState(false);
  const [confirmationScope, setConfirmationScope] = useState(scopeKey);
  const validityCallback = useRef(onValidityChange);
  useEffect(() => { validityCallback.current = onValidityChange; }, [onValidityChange]);
  const hasCandidates = accountIds.length > 0 || includedProviderGroupIds.length > 0;
  const customAllowed = accountIds.length > 0 && includedProviderGroupIds.length === 0 && excludedProviderGroupIds.length === 0;
  // A provider/account query is local provenance filtering, not a model-ID q.
  const sourceSearch = searchQuery.trim().toLowerCase();
  const matchingSourceIds = new Set(upstreams.filter(account => sourceSearch && syncAccountIds.includes(account.id)
    && [account.driver, account.name, account.id, providers.find(provider => provider.id === account.driver)?.display_name ?? ''].some(text => text.toLowerCase().includes(sourceSearch))).map(account => account.id));
  const matchesSource = matchingSourceIds.size > 0;
  const browseModelQuery = matchesSource ? '' : searchQuery.trim();
  const browseScopeKey = JSON.stringify([catalogKey, browseModelQuery]);
  const provenanceScopeKey = JSON.stringify([browseScopeKey, sourceSearch]);
  const boundedAccountIds = [...new Set(syncAccountIds)]
    .sort((left, right) => Number(matchingSourceIds.has(right)) - Number(matchingSourceIds.has(left)) || left.localeCompare(right))
    .slice(0, 8);
  const browseCatalog = browseResult?.scopeKey === browseScopeKey ? browseResult.data : undefined;
  const accountCatalogs = provenance?.scopeKey === provenanceScopeKey ? provenance.catalogs : new Map<string, AccountCatalog>();

  useEffect(() => {
    setSyncMessage('');
    return () => { syncController.current?.abort(); };
  }, [scopeKey]);

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
        if (!controller.signal.aborted) setBrowseCatalog({ scopeKey: browseScopeKey, data: result });
      } catch (reason) {
        if (!controller.signal.aborted) setBrowseError(reason instanceof Error ? reason.message : t('routes.catalogFailed'));
      } finally { if (!controller.signal.aborted) setBrowsing(false); }
    }, 250);
    return () => { window.clearTimeout(timeout); controller.abort(); };
  }, [open, browseScopeKey, refreshVersion]);

  useEffect(() => {
    // Render-time key equality, not this passive reset, guards old results.
    if (confirmationScope !== scopeKey) {
      setConfirmationScope(scopeKey); setCustomConfirmed(false); setPartialConfirmed(false);
    }
    setCatalog(undefined);
    if (!token || !tenant || !hasCandidates) { setCatalog(undefined); setError(''); setLoading(false); return; }
    const controller = new AbortController();
    setLoading(true); setError('');
    const timeout = window.setTimeout(async () => {
      const query = new URLSearchParams({ tenant_external_id: tenant, limit: '100' });
      if (accountIds.length) query.set('account_ids', accountIds.join(','));
      if (includedProviderGroupIds.length) query.set('include_provider_group_ids', includedProviderGroupIds.join(','));
      if (excludedProviderGroupIds.length) query.set('exclude_provider_group_ids', excludedProviderGroupIds.join(','));
      if (value.trim()) query.set('q', value.trim());
      setLoading(true); setError('');
      try {
        const data = await api<AggregateCatalog>(`/internal/v1/upstream-models?${query}`, token, { signal: controller.signal });
        if (!controller.signal.aborted) setCatalog({ scopeKey: catalogKey, data });
      }
      catch (reason) { if (!controller.signal.aborted) { setCatalog(undefined); setError(reason instanceof Error ? reason.message : t('routes.catalogFailed')); } }
      finally { if (!controller.signal.aborted) setLoading(false); }
    }, 250);
    return () => { window.clearTimeout(timeout); controller.abort(); };
  }, [scopeKey, refreshVersion]);

  useEffect(() => {
    let current = true;
    const controller = new AbortController();
    setProvenance(undefined); setProvenanceLoading(false);
    if (!open || !token || !tenant || !browseCatalog?.data.length) return;
    // There is no batch provenance API. Inspect at most eight candidates;
    // larger pools retain an explicit unknown row, never implied completeness.
    const ids = boundedAccountIds;
    setProvenanceLoading(ids.length > 0);
    const catalogScope = new URLSearchParams({ tenant_external_id: tenant, limit: '200' });
    if (browseModelQuery) catalogScope.set('q', browseModelQuery);
    let cursor = 0;
    const read = async () => {
      while (current && cursor < ids.length) {
        const accountId = ids[cursor++];
        try {
          const next = await api<AccountCatalog>(`/internal/v1/upstreams/${encodeURIComponent(accountId)}/models?${catalogScope}`, token, { signal: controller.signal });
          if (current) setProvenance((previous) => ({ scopeKey: provenanceScopeKey, catalogs: new Map(previous?.scopeKey === provenanceScopeKey ? previous.catalogs : []).set(accountId, next) }));
        } catch {
          // Missing account provenance stays explicitly unknown; aggregate
          // coverage and custom/partial-model validation are never inferred.
        }
      }
    };
    const timeout = window.setTimeout(() => {
      void Promise.all(Array.from({ length: Math.min(4, ids.length) }, read)).finally(() => { if (current) setProvenanceLoading(false); });
    }, 250);
    return () => { current = false; controller.abort(); window.clearTimeout(timeout); };
  }, [open, provenanceScopeKey, browseCatalog, refreshVersion]);

  const options = useMemo(() => ((open ? browseCatalog : catalog)?.data ?? []).filter((model) => model.protocol === protocol || model.protocol === 'any'), [open, browseCatalog, catalog, protocol]);
  const selected = catalog?.data.find((model) => model.id === value.trim() && (model.protocol === protocol || model.protocol === 'any'));
  const catalogFresh = Boolean(catalog && catalog.unknown_account_count === 0 && catalog.stale_account_count === 0);
  const selectedValid = Boolean(selected && catalogFresh && (selected.complete_coverage || (confirmationScope === scopeKey && partialConfirmed)));
  // A model returned by a stale or incomplete catalog is not verified. For
  // an exact account selection, keep the explicit custom-model escape hatch
  // available instead of leaving the form in a state with neither a usable
  // confirmation nor a valid submit button while synchronization settles.
  const needsCustomConfirmation = Boolean(value.trim() && (!selected || !catalogFresh));
  const allowCustom = Boolean(!syncing && needsCustomConfirmation && customAllowed && confirmationScope === scopeKey && customConfirmed);
  const valid = Boolean(!syncing && (selectedValid || allowCustom));
  useEffect(() => validityCallback.current({ scopeKey, valid, allowCustom }), [scopeKey, valid, allowCustom]);

  const choose = (model: CatalogModel) => {
    onChange(model.id); setCustomConfirmed(false); setPartialConfirmed(false);
  };
  const sync = async () => {
    if (!token || !tenant || boundedAccountIds.length === 0 || syncController.current) return;
    const controller = new AbortController();
    syncController.current = controller;
    const current = () => syncController.current === controller && latestScope.current === scopeKey && !controller.signal.aborted;
    // Eight accounts, two workers, one POST + at most four GETs each:
    // <= 40 requests and <= 2 in flight, regardless of candidate-pool size.
    const ids = boundedAccountIds;
    const deadline = window.setTimeout(() => controller.abort(), 30_000);
    // Notify the parent's synchronous submit guard before the first POST.
    // The epoch also rejects any pre-sync GET that resolves during refresh.
    validityCallback.current({ scopeKey, valid: false, allowCustom: false });
    setCatalog(undefined); setCustomConfirmed(false); setPartialConfirmed(false);
    setRefreshVersion((version) => version + 1);
    setSyncing(true); setError(''); setSyncMessage('');
    try {
      const query = new URLSearchParams({ tenant_external_id: tenant });
      let cursor = 0;
      let completed = 0;
      const worker = async () => {
        while (current() && cursor < ids.length) {
          const accountId = encodeURIComponent(ids[cursor++]);
          try {
            let accountCatalog = await api<AccountCatalog>(`/internal/v1/upstreams/${accountId}/models/sync?${query}`, token, { method: 'POST', signal: controller.signal });
            for (let attempt = 0; current() && accountCatalog.status === 'syncing' && attempt < 4; attempt += 1) {
              await delay(250, controller.signal);
              if (!current()) return;
              accountCatalog = await api<AccountCatalog>(`/internal/v1/upstreams/${accountId}/models?${query}`, token, { signal: controller.signal });
            }
            if (current() && accountCatalog.status === 'ready') completed += 1;
          } catch {
            // Failed, cancelled and still-syncing accounts remain unknown.
          }
        }
      };
      await Promise.all(Array.from({ length: Math.min(2, ids.length) }, worker));
      if (!current()) {
        if (latestScope.current === scopeKey && syncController.current === controller) {
          setSyncMessage(`${t('routes.syncModelsFailed')} · ${t('modelPicker.unknown')}: ${formatNumber(syncAccountIds.length, locale)}`);
        }
        return;
      }
      setSyncMessage(`${t('routes.syncModelsComplete', { count: formatNumber(completed, locale) })} · ${t('modelPicker.unknown')}: ${formatNumber(syncAccountIds.length - completed, locale)}`);
    } finally {
      window.clearTimeout(deadline);
      if (syncController.current === controller) {
        if (latestScope.current === scopeKey) setRefreshVersion((version) => version + 1);
        syncController.current = undefined; setSyncing(false);
      }
    }
  };
  const groupedOptions: ModelPickerOption[] = options.flatMap((model) => {
    const accounts = upstreams.filter((account) => syncAccountIds.includes(account.id) && accountCatalogs.get(account.id)?.models?.some((item) => item.id === model.id && (item.protocol === protocol || item.protocol === 'any')));
    const provenanceIncomplete = model.supported_account_count > accounts.length;
    return [...accounts, ...(provenanceIncomplete || !accounts.length ? [undefined] : [])].map((account) => ({
      key: `${account?.id ?? 'unknown'}:${model.protocol}:${model.id}`, value: model.id, label: model.id,
      provider: (account && (providers.find(provider => provider.id === account.driver)?.display_name || account.driver)) || t('modelPicker.unknown'), upstream: account?.name || t('modelPicker.unknown'),
      description: [
        model.complete_coverage ? t('routes.fullCoverage') : t('routes.partialCoverage', { supported: formatNumber(model.supported_account_count, locale), eligible: formatNumber(model.eligible_account_count, locale) }),
        model.context_window ? t('routes.contextWindow', { count: formatNumber(model.context_window, locale) }) : '',
        model.reservation_token_bound ? t('routes.reservationBound', { count: formatNumber(model.reservation_token_bound, locale) }) : '',
      ].filter(Boolean).join(' · '),
    }));
  });

  return <div className="model-combobox">
    <ModelPicker label={t('routes.upstreamModel')} editable searchableEditable invalid={!valid && Boolean(value.trim())} value={value} options={groupedOptions}
      loading={open ? browsing || provenanceLoading : loading} error={open ? browseError : error} onQueryChange={setSearchQuery} onOpen={() => setOpen(true)} onClose={() => setOpen(false)} onChange={(next) => {
      const option = options.find((model) => model.id === next);
      if (option) choose(option);
      else { onChange(next); setCustomConfirmed(false); setPartialConfirmed(false); }
    }} />
    {open && <small className="field-hint">{t('routing.catalogSearchHint')}</small>}
    <div className="catalog-status"><small className="field-hint">{loading || syncing ? t('routes.catalogLoading') : error || syncMessage || (catalog ? t('routes.catalogCoverage', { eligible: formatNumber(catalog.eligible_account_count, locale), unknown: formatNumber(catalog.unknown_account_count, locale), stale: formatNumber(catalog.stale_account_count, locale) }) : t('routes.selectCandidatesFirst'))}</small>{syncAccountIds.length > 0 && <button type="button" className="secondary" disabled={loading || syncing} onClick={() => void sync()}>{t('routes.syncModels')} ({formatNumber(boundedAccountIds.length, locale)} / {formatNumber(syncAccountIds.length, locale)})</button>}{syncing && <button type="button" className="secondary" onClick={() => syncController.current?.abort()}>{t('common.cancel')}</button>}</div>
    {syncAccountIds.length > boundedAccountIds.length && <small className="field-hint">{t('modelPicker.unknown')}: {formatNumber(syncAccountIds.length - boundedAccountIds.length, locale)}</small>}
    {selected && !selected.complete_coverage && <div className="custom-model-confirm"><label><input type="checkbox" checked={confirmationScope === scopeKey && partialConfirmed} onChange={(event) => { setConfirmationScope(scopeKey); setPartialConfirmed(event.target.checked); setCustomConfirmed(false); }} />{t('routes.confirmPartialCoverage', { supported: formatNumber(selected.supported_account_count, locale), eligible: formatNumber(selected.eligible_account_count, locale) })}</label></div>}
    {selected && catalog && (catalog.unknown_account_count > 0 || catalog.stale_account_count > 0) && <div className="notice warning compact">{t('routes.catalogNotReady')}</div>}
    {needsCustomConfirmation && <div className={`custom-model-confirm${customAllowed ? '' : ' disabled'}`}>
      {customAllowed ? <label><input type="checkbox" checked={confirmationScope === scopeKey && customConfirmed} onChange={(event) => { setConfirmationScope(scopeKey); setCustomConfirmed(event.target.checked); setPartialConfirmed(false); }} />{t('routes.confirmCustomModel', { model: value.trim() })}</label> : <span>{t('routes.customUnavailableForGroups')}</span>}
    </div>}
  </div>;
}
