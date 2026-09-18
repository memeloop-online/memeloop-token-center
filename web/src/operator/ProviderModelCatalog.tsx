import { useEffect, useRef, useState } from 'react';
import { api } from '../api';
import { Button } from '../design-system';
import { formatNumber } from '../format';
import { useI18n } from '../i18n';
import type { ModelPriceSyncResult } from '../types';

interface Catalog {
  status: string;
  error_code?: string | null;
  last_success_at?: number | null;
  models: Array<{ id: string; protocol: string }>;
}

export function ProviderModelCatalog({ accountId, tenant, token, disabled }: {
  accountId: string; tenant: string; token: string; disabled?: boolean;
}) {
  const { locale, t } = useI18n();
  const [catalog, setCatalog] = useState<Catalog>();
  const [busy, setBusy] = useState(false);
  const [catalogMessage, setCatalogMessage] = useState('');
  const [catalogError, setCatalogError] = useState('');
  const [priceMessage, setPriceMessage] = useState('');
  const [priceError, setPriceError] = useState('');
  const controller = useRef<AbortController | undefined>(undefined);
  const query = new URLSearchParams({ tenant_external_id: tenant, limit: '10000' });
  const path = `/internal/v1/upstreams/${accountId}/models`;
  const errorText = (reason: unknown) => reason instanceof Error ? reason.message : t('common.requestFailed');
  const catalogFailure = (value: Catalog) => {
    const key = `providerCatalog.error.${value.error_code || 'unavailable'}`;
    const message = t(key);
    return message === key ? t('providerCatalog.error.unavailable') : message;
  };

  useEffect(() => {
    controller.current?.abort();
    const read = new AbortController(); controller.current = read;
    setCatalog(undefined); setCatalogMessage(''); setCatalogError(''); setPriceMessage(''); setPriceError('');
    if (!token || !tenant) { setBusy(false); return () => read.abort(); }
    setBusy(true);
    void api<Catalog>(`${path}?${query}`, token, { signal: read.signal })
      .then(value => { if (!read.signal.aborted) setCatalog(value); })
      .catch(reason => { if (!read.signal.aborted) setCatalogError(errorText(reason)); })
      .finally(() => { if (!read.signal.aborted) setBusy(false); });
    return () => { read.abort(); controller.current?.abort(); };
  }, [accountId, tenant, token]);

  async function sync() {
    controller.current?.abort();
    const operation = new AbortController(); controller.current = operation;
    const signal = operation.signal;
    setBusy(true); setCatalogError(''); setPriceError(''); setPriceMessage('');
    setCatalogMessage(t('providerCatalog.syncingModels'));
    let current: Catalog;
    try {
      const result = await api<Catalog>(`${path}/sync?${query}`, token, { method: 'POST', signal });
      if (signal.aborted) return;
      setCatalog(result);
      if (result.status !== 'ready' || result.error_code) {
        setCatalogMessage('');
        if (result.status === 'syncing') setCatalogMessage(t('providerCatalog.inProgress'));
        else setCatalogError(catalogFailure(result));
        setBusy(false); return;
      }
      current = await api<Catalog>(`${path}?${query}`, token, { signal });
      if (signal.aborted) return;
      setCatalog(current);
      if (current.error_code || current.status !== 'ready') throw new Error(catalogFailure(current));
      setCatalogMessage(t('providerCatalog.modelsDone', { count: formatNumber(current.models.length, locale) }));
    } catch (reason) {
      if (!signal.aborted) { setCatalogMessage(''); setCatalogError(errorText(reason)); setBusy(false); }
      return;
    }
    const models = [...new Set(current.models.map(model => model.id))];
    if (!models.length) { setPriceMessage(t('providerCatalog.empty')); setBusy(false); return; }
    let imported = 0; let preserved = 0; let pending = 0; let processed = 0;
    const failedSources = new Set<string>();
    try {
      for (let offset = 0; offset < models.length; offset += 500) {
        setPriceMessage(t('providerCatalog.pricingProgress', { done: formatNumber(offset, locale), total: formatNumber(models.length, locale) }));
        const result = await api<ModelPriceSyncResult>('/internal/v1/model-prices/sync', token, {
          method: 'POST', signal,
          body: JSON.stringify({ models: models.slice(offset, offset + 500), currency: 'USD', tenant_external_id: tenant }),
        });
        if (signal.aborted) return;
        imported += result.imported; preserved += result.preserved.length;
        pending += result.unmatched.length + result.candidates.length;
        processed += Math.min(500, models.length - offset);
        for (const source of result.sourceResults ?? []) if (source.error) failedSources.add(source.source);
      }
      setPriceMessage(t('providerCatalog.pricesDone', { imported: formatNumber(imported, locale), preserved: formatNumber(preserved, locale), pending: formatNumber(pending, locale) }));
      if (failedSources.size) setPriceError(t('providerCatalog.sourcesFailed', { sources: [...failedSources].join('、') }));
    } catch (reason) {
      if (!signal.aborted) {
        setPriceMessage(t('providerCatalog.pricesDone', { imported: formatNumber(imported, locale), preserved: formatNumber(preserved, locale), pending: formatNumber(pending + models.length - processed, locale) }));
        setPriceError(errorText(reason));
      }
    } finally { if (!signal.aborted) setBusy(false); }
  }

  const status = catalog && ['ready', 'stale', 'syncing', 'error', 'unknown'].includes(catalog.status) ? catalog.status : 'unknown';
  const visibleCatalogError = catalogError || (catalog?.error_code ? catalogFailure(catalog) : '');
  return <section className="provider-model-catalog" aria-label={t('providerCatalog.title')} aria-busy={busy}>
    <div className="row-actions"><h4>{t('providerCatalog.title')}</h4><Button appearance="secondary" type="button" disabled={disabled || busy || !token || !tenant} onClick={() => void sync()}>{t('providerCatalog.sync')}</Button></div>
    <p className="muted">{busy && !catalog ? t('common.loading') : t(`providerCatalog.status.${status}`)}{catalog && <> · {t('providerCatalog.count', { count: formatNumber(catalog.models.length, locale) })}</>}{catalog?.last_success_at && <> · {new Date(catalog.last_success_at).toLocaleString(locale)}</>}</p>
    {catalogMessage && <p role="status">{catalogMessage}</p>}
    {visibleCatalogError && <p className="error" role="alert">{t('providerCatalog.modelsLabel')}: {visibleCatalogError}</p>}
    {priceMessage && <p role="status">{t('providerCatalog.pricesLabel')}: {priceMessage}</p>}
    {priceError && <p className="error" role="alert">{t('providerCatalog.pricesLabel')}: {priceError}</p>}
  </section>;
}
