import { useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { api } from '../api';
import { formatNumber } from '../format';
import { useI18n } from '../i18n';
import { ModelPicker, type ModelPickerOption } from '../ModelPicker';
import type { UpstreamAccount } from '../types';
import { confirmationForScope, modelConfirmationValidity } from './modelConfirmation';

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
  const sourceKey = JSON.stringify([accountIds, includedProviderGroupIds, excludedProviderGroupIds, syncAccountIds, protocol]);
  const confirmationScope = JSON.stringify([token, tenant, sourceKey, value]);
  const [catalogResult, setCatalogResult] = useState<{ scope: string; data: AggregateCatalog }>();
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');
  const [syncMessage, setSyncMessage] = useState('');
  const [open, setOpen] = useState(false);
  const [accountCatalogs, setAccountCatalogs] = useState<Map<string, AccountCatalog>>(new Map());
  // Persisted consent applies only to the scope first opened for editing.
  // Never restore that prop after an account, group, membership or model change.
  const [customConfirmation, setCustomConfirmation] = useState({ scope: confirmationScope, confirmed: customModelConfirmed });
  const scopedConfirmation = confirmationForScope(customConfirmation, confirmationScope);
  const customConfirmed = scopedConfirmation.confirmed;
  const setCustomConfirmed = (confirmed: boolean) => setCustomConfirmation({ scope: confirmationScope, confirmed });
  useLayoutEffect(() => {
    if (customConfirmation.scope !== confirmationScope) setCustomConfirmation(scopedConfirmation);
  }, [confirmationScope, customConfirmation.scope]);
  const [refreshVersion, setRefreshVersion] = useState(0);
  const catalogScope = JSON.stringify([confirmationScope, refreshVersion]);
  const catalog = catalogResult?.scope === catalogScope ? catalogResult.data : undefined;
  const validityCallback = useRef(onValidityChange);
  useLayoutEffect(() => { validityCallback.current = onValidityChange; }, [onValidityChange]);
  const hasCandidates = accountIds.length > 0 || includedProviderGroupIds.length > 0;
  const customAllowed = accountIds.length > 0 && includedProviderGroupIds.length === 0 && excludedProviderGroupIds.length === 0;
  const hasExplicitCodexOAuth = upstreams.some((account) => accountIds.includes(account.id)
    && account.driver === 'openai-codex' && account.connection_method === 'oauth');

  useEffect(() => {
    // The scope check above hides the previous result immediately, before
    // this effect or the debounced read runs. A new candidate set must never
    // borrow the previous catalog's coverage or suppress its own confirmation.
    setCatalogResult(undefined);
    if (!token || !tenant || !hasCandidates) { setError(''); setLoading(false); return; }
    const controller = new AbortController();
    const timeout = window.setTimeout(async () => {
      const query = new URLSearchParams({ tenant_external_id: tenant, limit: '100' });
      if (accountIds.length) query.set('account_ids', accountIds.join(','));
      if (includedProviderGroupIds.length) query.set('include_provider_group_ids', includedProviderGroupIds.join(','));
      if (excludedProviderGroupIds.length) query.set('exclude_provider_group_ids', excludedProviderGroupIds.join(','));
      if (value.trim()) query.set('q', value.trim());
      setLoading(true); setError('');
      try {
        const data = await api<AggregateCatalog>(`/internal/v1/upstream-models?${query}`, token, { signal: controller.signal });
        if (!controller.signal.aborted) setCatalogResult({ scope: catalogScope, data });
      }
      catch (reason) { if (!controller.signal.aborted) { setCatalogResult(undefined); setError(reason instanceof Error ? reason.message : t('routes.catalogFailed')); } }
      finally { if (!controller.signal.aborted) setLoading(false); }
    }, 250);
    return () => { window.clearTimeout(timeout); controller.abort(); };
  }, [token, tenant, sourceKey, value, refreshVersion]);

  useEffect(() => {
    let current = true;
    setAccountCatalogs(new Map());
    if (!open || !token || !tenant) return;
    const ids = [...new Set(syncAccountIds)];
    const catalogScope = new URLSearchParams({ tenant_external_id: tenant, limit: '200' });
    if (value.trim()) catalogScope.set('q', value.trim());
    let cursor = 0;
    const read = async () => {
      while (current && cursor < ids.length) {
        const accountId = ids[cursor++];
        try {
          const next = await api<AccountCatalog>(`/internal/v1/upstreams/${encodeURIComponent(accountId)}/models?${catalogScope}`, token);
          if (current) setAccountCatalogs((catalogs) => new Map(catalogs).set(accountId, next));
        } catch {
          // Missing account provenance stays explicitly unknown. Aggregate
          // catalog listing remains authoritative for validation.
        }
      }
    };
    const timeout = window.setTimeout(() => {
      void Promise.all(Array.from({ length: Math.min(4, ids.length) }, read));
    }, 250);
    return () => { current = false; window.clearTimeout(timeout); };
  }, [open, token, tenant, sourceKey, value, refreshVersion]);

  const options = useMemo(() => (catalog?.data ?? []).filter((model) => (model.protocol === protocol || model.protocol === 'any')
    && (!value.trim() || model.id.toLowerCase().includes(value.trim().toLowerCase()))), [catalog, protocol, value]);
  const selected = catalog?.data.find((model) => model.id === value && (model.protocol === protocol || model.protocol === 'any'));
  // The runtime accepts a current-generation snapshot while it is ready or
  // stale, and excludes accounts without a matching catalog entry. Mirror
  // that safe restriction here. A stale or partial snapshot must not turn a
  // discovered model into an explicit_custom bypass.
  const { needsCustomConfirmation, allowCustom, valid } = modelConfirmationValidity({
    hasValue: Boolean(value.trim()), catalogListed: Boolean(selected), customAllowed, customConfirmed,
  });
  useLayoutEffect(() => validityCallback.current(valid, allowCustom), [valid, allowCustom]);

  const choose = (model: CatalogModel) => {
    onChange(model.id); setCustomConfirmed(false);
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
        model.complete_coverage ? t('routes.catalogVerified') : t('routes.catalogRestricted'),
        model.context_window ? t('routes.contextWindow', { count: formatNumber(model.context_window, locale) }) : '',
        model.reservation_token_bound ? t('routes.reservationBound', { count: formatNumber(model.reservation_token_bound, locale) }) : '',
      ].filter(Boolean).join(' · '),
    }));
  });

  return <div className="model-combobox">
    <ModelPicker label={t('routes.upstreamModel')} editable invalid={!valid && Boolean(value.trim())} value={value} options={groupedOptions} loading={loading} error={error} onOpen={() => setOpen(true)} onChange={(next) => {
      const option = options.find((model) => model.id === next);
      if (option) choose(option);
      else { onChange(next); setCustomConfirmed(false); }
    }} />
    <div className="catalog-status"><small className="field-hint">{loading ? t('routes.catalogLoading') : error || syncMessage || (selected ? (catalog && catalog.stale_account_count > 0 ? t('routes.catalogLastVerified') : selected.complete_coverage ? t('routes.catalogVerified') : t('routes.catalogRestricted')) : catalog ? t('routes.catalogReady') : t('routes.selectCandidatesFirst'))}</small>{syncAccountIds.length > 0 && <button type="button" className="secondary" disabled={loading} onClick={() => void sync()}>{t('routes.syncModels')}</button>}</div>
    {needsCustomConfirmation && customAllowed && <div className="notice warning compact">{t('routes.catalogUnverified')}{hasExplicitCodexOAuth && <> {t('routes.codexCapabilityHint')}</>}</div>}
    {needsCustomConfirmation && <div className={`custom-model-confirm${customAllowed ? '' : ' disabled'}`}>
      {customAllowed ? <label><input type="checkbox" checked={customConfirmed} onChange={(event) => setCustomConfirmed(event.target.checked)} />{t('routes.confirmCustomModel', { model: value.trim() })}</label> : <span>{t('routes.customUnavailableForGroups')}</span>}
    </div>}
  </div>;
}
