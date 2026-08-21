import { useEffect, useId, useMemo, useRef, useState, type KeyboardEvent } from 'react';
import { api } from '../api';
import { formatNumber } from '../format';
import { useI18n } from '../i18n';

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
}

export function UpstreamModelCombobox({ token, tenant, accountIds, includedProviderGroupIds, excludedProviderGroupIds, syncAccountIds, protocol, value, onChange, customModelConfirmed, onValidityChange }: Props) {
  const { locale, t } = useI18n();
  const id = useId();
  const [catalog, setCatalog] = useState<AggregateCatalog>();
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');
  const [syncMessage, setSyncMessage] = useState('');
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(-1);
  const [customConfirmed, setCustomConfirmed] = useState(customModelConfirmed);
  const [partialConfirmed, setPartialConfirmed] = useState(false);
  const [refreshVersion, setRefreshVersion] = useState(0);
  const validityCallback = useRef(onValidityChange);
  useEffect(() => { validityCallback.current = onValidityChange; }, [onValidityChange]);
  const sourceKey = `${accountIds.join(',')}|${includedProviderGroupIds.join(',')}|${excludedProviderGroupIds.join(',')}|${protocol}`;
  const hasCandidates = accountIds.length > 0 || includedProviderGroupIds.length > 0;
  const customAllowed = accountIds.length > 0 && includedProviderGroupIds.length === 0 && excludedProviderGroupIds.length === 0;

  useEffect(() => {
    setCustomConfirmed(customModelConfirmed);
    setPartialConfirmed(false);
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

  const options = useMemo(() => (catalog?.data ?? []).filter((model) => (model.protocol === protocol || model.protocol === 'any')
    && (!value.trim() || model.id.toLowerCase().includes(value.trim().toLowerCase()))), [catalog, protocol, value]);
  const selected = catalog?.data.find((model) => model.id === value && (model.protocol === protocol || model.protocol === 'any'));
  const catalogFresh = Boolean(catalog && catalog.unknown_account_count === 0 && catalog.stale_account_count === 0);
  const selectedValid = Boolean(selected && catalogFresh && (selected.complete_coverage || partialConfirmed));
  const valid = Boolean(selectedValid || (value.trim() && customAllowed && customConfirmed));
  const allowCustom = Boolean(!selected && customAllowed && customConfirmed);
  useEffect(() => validityCallback.current(valid, allowCustom), [valid, allowCustom]);

  const choose = (model: CatalogModel) => {
    onChange(model.id); setCustomConfirmed(false); setPartialConfirmed(false); setOpen(false); setActive(-1);
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
  const onKeyDown = (event: KeyboardEvent<HTMLInputElement>) => {
    if (event.key === 'ArrowDown') { event.preventDefault(); setOpen(true); setActive((current) => Math.min(current + 1, Math.max(options.length - 1, 0))); }
    else if (event.key === 'ArrowUp') { event.preventDefault(); setActive((current) => Math.max(current - 1, 0)); }
    else if (event.key === 'Enter' && open && options[active >= 0 ? active : 0]) { event.preventDefault(); choose(options[active >= 0 ? active : 0]); }
    else if (event.key === 'Escape') { event.preventDefault(); setOpen(false); }
  };

  return <div className="model-combobox">
    <label htmlFor={`${id}-input`}>{t('routes.upstreamModel')}</label>
    <input id={`${id}-input`} role="combobox" aria-autocomplete="list" aria-expanded={open} aria-controls={`${id}-list`} aria-activedescendant={open && active >= 0 && options[active] ? `${id}-option-${active}` : undefined} aria-invalid={!valid && Boolean(value.trim())} autoComplete="off" value={value} onFocus={() => setOpen(true)} onBlur={() => window.setTimeout(() => setOpen(false), 100)} onKeyDown={onKeyDown} onChange={(event) => { onChange(event.target.value); setCustomConfirmed(false); setActive(-1); setOpen(true); }} />
    <div className="catalog-status"><small className="field-hint">{loading ? t('routes.catalogLoading') : error || syncMessage || (catalog ? t('routes.catalogCoverage', { eligible: formatNumber(catalog.eligible_account_count, locale), unknown: formatNumber(catalog.unknown_account_count, locale), stale: formatNumber(catalog.stale_account_count, locale) }) : t('routes.selectCandidatesFirst'))}</small>{syncAccountIds.length > 0 && <button type="button" className="secondary" disabled={loading} onClick={() => void sync()}>{t('routes.syncModels')}</button>}</div>
    {open && options.length > 0 && <div className="combobox-popover model-options" id={`${id}-list`} role="listbox">{options.map((model, index) => <button type="button" role="option" aria-selected={index === active} className={index === active ? 'active' : ''} id={`${id}-option-${index}`} key={`${model.protocol}:${model.id}`} onMouseDown={(event) => event.preventDefault()} onMouseEnter={() => setActive(index)} onClick={() => choose(model)}><span><b>{model.id}</b><small>{model.complete_coverage ? t('routes.fullCoverage') : t('routes.partialCoverage', { supported: formatNumber(model.supported_account_count, locale), eligible: formatNumber(model.eligible_account_count, locale) })}</small></span><span className="model-limits">{model.context_window ? t('routes.contextWindow', { count: formatNumber(model.context_window, locale) }) : ''}{model.reservation_token_bound ? t('routes.reservationBound', { count: formatNumber(model.reservation_token_bound, locale) }) : ''}</span></button>)}</div>}
    {selected && !selected.complete_coverage && <div className="custom-model-confirm"><label><input type="checkbox" checked={partialConfirmed} onChange={(event) => setPartialConfirmed(event.target.checked)} />{t('routes.confirmPartialCoverage', { supported: formatNumber(selected.supported_account_count, locale), eligible: formatNumber(selected.eligible_account_count, locale) })}</label></div>}
    {selected && catalog && (catalog.unknown_account_count > 0 || catalog.stale_account_count > 0) && <div className="notice warning compact">{t('routes.catalogNotReady')}</div>}
    {value.trim() && !selected && <div className={`custom-model-confirm${customAllowed ? '' : ' disabled'}`}>
      {customAllowed ? <label><input type="checkbox" checked={customConfirmed} onChange={(event) => setCustomConfirmed(event.target.checked)} />{t('routes.confirmCustomModel', { model: value.trim() })}</label> : <span>{t('routes.customUnavailableForGroups')}</span>}
    </div>}
  </div>;
}
