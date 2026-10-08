import { useLayoutEffect, useRef, useState } from 'react';
import { api, ApiError } from '../api';
import { Button } from '../design-system';
import { formatNumber } from '../format';
import { useI18n } from '../i18n';
import type { ManagedModelSyncResponse } from '../types';
import { ManagedSyncResponseError, managedSyncFeedback, parseManagedModelSync } from './managedModelSync';

interface SyncState {
  scope: string;
  phase: 'idle' | 'syncing' | 'done' | 'failed';
  result?: ManagedModelSyncResponse;
  message?: string;
  retryable?: boolean;
}

/** First-screen per-account sync action: refreshes the complete catalog and reconciles managed routes. */
export function ManagedModelSync({ accountId, tenant, token, disabled = false, onReconciled, onReviewModels, onReviewPricing, reviewModelsDisabled = false, reviewPricingDisabled = false }: {
  accountId: string;
  tenant: string;
  token: string;
  disabled?: boolean;
  onReconciled?: () => void;
  onReviewModels?: () => void;
  onReviewPricing?: () => void;
  reviewModelsDisabled?: boolean;
  reviewPricingDisabled?: boolean;
}) {
  const { locale, t } = useI18n();
  const scope = JSON.stringify([accountId, tenant, token]);
  const [state, setState] = useState<SyncState>({ scope, phase: 'idle' });
  const request = useRef<AbortController | undefined>(undefined);
  useLayoutEffect(() => {
    if (state.scope === scope) return;
    request.current?.abort();
    request.current = undefined;
    setState({ scope, phase: 'idle' });
  }, [scope, state.scope]);
  useLayoutEffect(() => () => { request.current?.abort(); }, []);

  const visible = state.scope === scope ? state : { scope, phase: 'idle' } as SyncState;
  const syncing = visible.phase === 'syncing';
  const result = visible.result;
  const feedback = result ? managedSyncFeedback(result) : undefined;
  const retry = visible.phase === 'failed' ? Boolean(visible.retryable) : Boolean(feedback?.retry);
  const priceSync = result?.price_sync;
  const priceSummary = priceSync && priceSync.status !== 'deferred' ? t('managedSync.price.summary', {
    imported: formatNumber(priceSync.imported, locale),
    preserved: formatNumber(priceSync.preserved, locale),
    unmatched: formatNumber(priceSync.unmatched, locale),
    ambiguous: formatNumber(priceSync.ambiguous, locale),
  }) : '';
  const failedSources = priceSync && priceSync.status !== 'deferred' && priceSync.failed_sources.length > 0
    ? t('managedSync.price.sources', {
      sources: new Intl.ListFormat(locale, { style: 'long', type: 'conjunction' }).format(priceSync.failed_sources),
    })
    : '';
  const priceHeading = priceSync && priceSync.status !== 'deferred' && priceSync.status !== 'error'
    ? priceSync.unmatched > 0 || priceSync.ambiguous > 0
      ? t(priceSync.unmatched > 0 && priceSync.ambiguous > 0 ? 'managedSync.price.needsReview'
        : priceSync.unmatched > 0 ? 'managedSync.price.unmatchedHeading' : 'managedSync.price.ambiguousHeading', {
        unmatched: formatNumber(priceSync.unmatched, locale), ambiguous: formatNumber(priceSync.ambiguous, locale),
      })
      : t(priceSync.failed_sources.length > 0 ? 'managedSync.price.sourcesUnavailable' : `managedSync.price.${priceSync.status}`)
    : priceSync ? t(`managedSync.price.${priceSync.status}`) : '';
  const warningText = (code: string) => {
    const key = `managedSync.warning.${code}`;
    const translated = t(key);
    if (translated !== key) return translated;
    const catalogKey = `providerCatalog.error.${code}`;
    const catalogTranslated = t(catalogKey);
    return catalogTranslated === catalogKey ? t('managedSync.warning.unknown') : catalogTranslated;
  };

  async function sync() {
    if (disabled || syncing || request.current || !token || !tenant) return;
    const controller = new AbortController();
    request.current = controller;
    setState({ scope, phase: 'syncing', result: visible.result });
    try {
      const query = new URLSearchParams({ tenant_external_id: tenant });
      const synced = parseManagedModelSync(await api<unknown>(`/internal/v1/upstreams/${accountId}/models/sync-routes?${query}`, token, { method: 'POST', signal: controller.signal }));
      if (controller.signal.aborted) return;
      setState({ scope, phase: 'done', result: synced });
      onReconciled?.();
    } catch (reason) {
      if (controller.signal.aborted) return;
      const key = reason instanceof ApiError && reason.code ? `providerCatalog.error.${reason.code}` : '';
      const translated = key ? t(key) : '';
      const message = reason instanceof ManagedSyncResponseError
        ? t('providerCatalog.error.invalid_response')
        : reason instanceof ApiError && (reason.status === 401 || reason.status === 403)
          ? t('managedSync.permissionDenied')
          : translated && translated !== key ? translated : t('managedSync.failed');
      const retryable = !(reason instanceof ApiError && (reason.status === 401 || reason.status === 403
        || reason.code === 'unsupported' || reason.code === 'credential_invalid' || reason.code === 'authentication_failed'));
      setState({ scope, phase: 'failed', result: visible.result, message, retryable });
    } finally {
      if (request.current === controller) request.current = undefined;
    }
  }

  return <div className="managed-model-sync" aria-busy={syncing}>
    <div className="managed-model-sync-row">
      <Button appearance="primary" type="button" disabled={disabled || syncing || !token || !tenant} onClick={() => void sync()}>{t(syncing ? 'managedSync.pending' : retry ? 'managedSync.retry' : 'managedSync.sync')}</Button>
      <span className="field-hint">{t('managedSync.hint')}</span>
    </div>
    {syncing && <p className="muted" role="status">{t('managedSync.syncing')}</p>}
    {result && <div className="managed-model-sync-result">
      <p role="status"><span className={`status ${feedback?.routesComplete ? 'ok' : 'pending'}`}>{t(feedback?.routesComplete ? 'managedSync.done' : 'managedSync.partial')}</span></p>
      {visible.phase !== 'done' && <p className="muted">{t('managedSync.previous')}</p>}
      <p>{t('managedSync.summary', {
        count: formatNumber(result.catalog.models.length, locale),
        added: formatNumber(result.routes.added, locale),
        disabled: formatNumber(result.routes.disabled, locale),
        restored: formatNumber(result.routes.restored, locale),
        unchanged: formatNumber(result.routes.unchanged, locale),
        skipped: formatNumber(result.routes.skipped, locale),
      })}</p>
      {priceSync?.status === 'deferred'
        ? <p className="muted">{t('managedSync.priceDeferred')}</p>
        : priceSync && <>
          <p className={priceSync.status === 'error' ? 'error' : 'muted'} role={priceSync.status === 'error' ? 'alert' : 'status'}>
            {priceHeading} · {priceSummary}
          </p>
          {failedSources && <p className={priceSync.status === 'error' ? 'error' : 'muted'}>{failedSources}</p>}
        </>}
      {priceSync && feedback?.reviewPrices && <p>{t(priceSync.status === 'deferred' ? 'managedSync.price.deferredHelp' : 'managedSync.price.reviewHelp')}</p>}
      {priceSync && priceSync.status !== 'deferred' && priceSync.preserved > 0 && <p className="muted">{t('managedSync.price.preservedHelp')}</p>}
      <div className="row-actions">
        {onReviewModels && <Button appearance="secondary" type="button" disabled={reviewModelsDisabled || syncing || !token || !tenant} onClick={() => { if (!reviewModelsDisabled && !syncing && token && tenant) onReviewModels(); }}>{t('managedSync.reviewModels')}</Button>}
        {onReviewPricing && feedback?.reviewPrices && <Button appearance="secondary" type="button" disabled={reviewPricingDisabled || syncing || !token || !tenant} onClick={() => { if (!reviewPricingDisabled && !syncing && token && tenant) onReviewPricing(); }}>{t('managedSync.reviewPricing')}</Button>}
      </div>
      {result.routes.warnings.length > 0 && <ul className="managed-model-sync-warnings">
        {result.routes.warnings.map((warning) => <li key={warning}>{warningText(warning)}</li>)}
      </ul>}
    </div>}
    {visible.phase === 'failed' && <p className="error" role="alert">{visible.message}</p>}
  </div>;
}
