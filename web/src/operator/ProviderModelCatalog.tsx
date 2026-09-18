import { useEffect, useRef, useState } from 'react';
import { api } from '../api';
import { Button, Disclosure } from '../design-system';
import { formatNumber } from '../format';
import { useI18n } from '../i18n';
import type { UpstreamCatalogPriceSync, UpstreamModelCatalogResponse, UpstreamModelCatalogSyncResponse } from '../types';

const maximumDisabledModelsShown = 100;

export function ProviderModelCatalog({ accountId, tenant, token, disabled }: {
  accountId: string; tenant: string; token: string; disabled?: boolean;
}) {
  const { locale, t } = useI18n();
  const [catalog, setCatalog] = useState<UpstreamModelCatalogResponse>();
  const [busy, setBusy] = useState(false);
  const [catalogMessage, setCatalogMessage] = useState('');
  const [catalogError, setCatalogError] = useState('');
  const [priceSync, setPriceSync] = useState<UpstreamCatalogPriceSync>();
  const controller = useRef<AbortController | undefined>(undefined);
  const query = new URLSearchParams({ tenant_external_id: tenant, limit: '10000' });
  const path = `/internal/v1/upstreams/${accountId}/models`;
  const errorText = (reason: unknown) => reason instanceof Error ? reason.message : t('common.requestFailed');
  const catalogFailure = (value: UpstreamModelCatalogResponse) => {
    const key = `providerCatalog.error.${value.error_code || 'unavailable'}`;
    const message = t(key);
    return message === key ? t('providerCatalog.error.unavailable') : message;
  };
  const parseCatalog = (value: unknown): UpstreamModelCatalogResponse => {
    if (!value || typeof value !== 'object' || !('account_id' in value) || typeof value.account_id !== 'string'
      || !('status' in value) || typeof value.status !== 'string'
      || !('credential_generation' in value) || typeof value.credential_generation !== 'number'
      || !('last_attempt_at' in value) || (value.last_attempt_at !== null && typeof value.last_attempt_at !== 'number')
      || !('last_success_at' in value) || (value.last_success_at !== null && typeof value.last_success_at !== 'number')
      || !('expires_at' in value) || (value.expires_at !== null && typeof value.expires_at !== 'number')
      || !('error_code' in value) || (value.error_code !== null && typeof value.error_code !== 'string')
      || !('models' in value) || !Array.isArray(value.models)
      || !value.models.every(model => model && typeof model.id === 'string' && typeof model.protocol === 'string')
      || !('disabled_models' in value) || !Array.isArray(value.disabled_models)
      || !value.disabled_models.every(model => model && typeof model.id === 'string' && typeof model.protocol === 'string'
        && model.status === 'disabled' && typeof model.disabled_at === 'number' && model.reason === 'removed_from_upstream')) {
      throw new Error(t('providerCatalog.error.invalid_response'));
    }
    return value as UpstreamModelCatalogResponse;
  };
  const parseSync = (value: unknown): UpstreamModelCatalogSyncResponse => {
    const catalog = parseCatalog(value);
    const priceSync = (value as Record<string, unknown>).price_sync;
    if (!priceSync || typeof priceSync !== 'object') {
      throw new Error(t('providerCatalog.error.invalid_response'));
    }
    const fields = priceSync as Record<string, unknown>;
    const failedSources = fields.failed_sources;
    if (typeof fields.status !== 'string' || !['ready', 'partial', 'error', 'skipped'].includes(fields.status)
      || fields.currency !== 'USD'
      || !['imported', 'preserved', 'unmatched', 'ambiguous'].every((field) => typeof fields[field] === 'number')
      || !Array.isArray(failedSources) || !failedSources.every((source) => typeof source === 'string')
      || (fields.error_code !== null && fields.error_code !== 'price_sync_failed')) {
      throw new Error(t('providerCatalog.error.invalid_response'));
    }
    return { ...catalog, price_sync: priceSync as UpstreamCatalogPriceSync };
  };

  useEffect(() => {
    controller.current?.abort();
    const read = new AbortController(); controller.current = read;
    setCatalog(undefined); setCatalogMessage(''); setCatalogError(''); setPriceSync(undefined);
    if (!token || !tenant) { setBusy(false); return () => read.abort(); }
    setBusy(true);
    void api<unknown>(`${path}?${query}`, token, { signal: read.signal }).then(parseCatalog)
      .then(value => { if (!read.signal.aborted) setCatalog(value); })
      .catch(reason => { if (!read.signal.aborted) setCatalogError(errorText(reason)); })
      .finally(() => { if (!read.signal.aborted) setBusy(false); });
    return () => { read.abort(); controller.current?.abort(); };
  }, [accountId, tenant, token]);

  async function sync() {
    controller.current?.abort();
    const operation = new AbortController(); controller.current = operation;
    const signal = operation.signal;
    setBusy(true); setCatalogError(''); setPriceSync(undefined);
    setCatalogMessage(t('providerCatalog.syncingModels'));
    try {
      const result = parseSync(await api<unknown>(`${path}/sync?${query}`, token, { method: 'POST', signal }));
      if (signal.aborted) return;
      setCatalog(result);
      setPriceSync(result.price_sync);
      if (result.status !== 'ready' || result.error_code) {
        setCatalogMessage('');
        if (result.status === 'syncing') setCatalogMessage(t('providerCatalog.inProgress'));
        else setCatalogError(catalogFailure(result));
        setBusy(false); return;
      }
      setCatalogMessage(t('providerCatalog.modelsDone', { count: formatNumber(result.models.length, locale) }));
    } catch (reason) {
      if (!signal.aborted) { setCatalogMessage(''); setCatalogError(errorText(reason)); setBusy(false); }
      return;
    }
    if (!signal.aborted) setBusy(false);
  }

  const status = catalog && ['ready', 'stale', 'syncing', 'error', 'unknown'].includes(catalog.status) ? catalog.status : 'unknown';
  const visibleCatalogError = catalogError || (catalog?.error_code ? catalogFailure(catalog) : '');
  const disabledModels = catalog?.disabled_models ?? [];
  const priceSummary = priceSync && t(`providerCatalog.priceSync.${priceSync.status}`, {
    imported: formatNumber(priceSync.imported, locale),
    preserved: formatNumber(priceSync.preserved, locale),
    unmatched: formatNumber(priceSync.unmatched, locale),
    ambiguous: formatNumber(priceSync.ambiguous, locale),
  });
  const priceFailure = priceSync?.status === 'error' ? t('providerCatalog.priceSync.errorDetail') : '';
  const failedSources = priceSync?.failed_sources.length
    ? t('providerCatalog.sourcesFailed', { sources: new Intl.ListFormat(locale, { style: 'long', type: 'conjunction' }).format(priceSync.failed_sources) })
    : '';
  return <section className="provider-model-catalog" aria-label={t('providerCatalog.title')} aria-busy={busy}>
    <div className="row-actions"><h4>{t('providerCatalog.title')}</h4><Button appearance="secondary" type="button" disabled={disabled || busy || !token || !tenant} onClick={() => void sync()}>{t('providerCatalog.sync')}</Button></div>
    <p className="muted">{busy && !catalog ? t('common.loading') : t(`providerCatalog.status.${status}`)}{catalog && <> · {t('providerCatalog.count', { count: formatNumber(catalog.models.length, locale) })}</>}{catalog?.last_success_at && <> · {new Date(catalog.last_success_at).toLocaleString(locale)}</>}</p>
    {catalogMessage && <p role="status">{catalogMessage}</p>}
    {visibleCatalogError && <p className="error" role="alert">{t('providerCatalog.modelsLabel')}: {visibleCatalogError}</p>}
    {priceSummary && <p role="status">{t('providerCatalog.pricesLabel')}: {priceSummary}</p>}
    {failedSources && <p className={priceSync?.status === 'partial' ? 'muted' : 'error'} role={priceSync?.status === 'partial' ? 'status' : 'alert'}>{t('providerCatalog.pricesLabel')}: {failedSources}</p>}
    {priceFailure && <p className="error" role="alert">{t('providerCatalog.pricesLabel')}: {priceFailure}</p>}
    {disabledModels.length > 0 && <Disclosure title={t('providerCatalog.disabledModels', { count: formatNumber(disabledModels.length, locale) })}>
      <p className="muted">{t('providerCatalog.disabledModelsHint')}</p>
      <ul>{disabledModels.slice(0, maximumDisabledModelsShown).map((model) => <li key={`${model.id}\u0000${model.protocol}`}><code>{model.id}</code> · {model.protocol} · {t('common.disabled')}</li>)}</ul>
      {disabledModels.length > maximumDisabledModelsShown && <p className="muted">{t('providerCatalog.disabledModelsRemaining', { count: formatNumber(disabledModels.length - maximumDisabledModelsShown, locale) })}</p>}
    </Disclosure>}
  </section>;
}
