import { useEffect, useRef, useState } from 'react';
import { api, apiRead } from '../api';
import { Button, Disclosure, Input } from '../design-system';
import { formatNumber } from '../format';
import { useI18n } from '../i18n';
import type { ModelRouteView, UpstreamCatalogPriceSync, UpstreamModelCatalogResponse, UpstreamModelCatalogSyncResponse } from '../types';
import { findManagedRoute, inferManagedRouteProtocol, isUpstreamModelCatalog, type CatalogRouteAction } from './managedModelSync';

const maximumDisabledModelsShown = 100;
const maximumCatalogModelsShown = 50;
const routePageSize = 100;
const maximumRoutesLoaded = 10_000;

export function ProviderModelCatalog({ accountId, tenant, token, disabled, onRouteAction, routeActionDisabled, routeCacheRevision = 0 }: {
  accountId: string; tenant: string; token: string; disabled?: boolean;
  onRouteAction?: (action: CatalogRouteAction) => void;
  routeActionDisabled?: boolean;
  routeCacheRevision?: number;
}) {
  const { locale, t } = useI18n();
  const [catalog, setCatalog] = useState<UpstreamModelCatalogResponse>();
  const [busy, setBusy] = useState(false);
  const [catalogMessage, setCatalogMessage] = useState('');
  const [catalogError, setCatalogError] = useState('');
  const [priceSync, setPriceSync] = useState<UpstreamCatalogPriceSync>();
  const [browseFilter, setBrowseFilter] = useState('');
  const [routes, setRoutes] = useState<ModelRouteView[]>();
  const [routesLoading, setRoutesLoading] = useState(false);
  const [routesError, setRoutesError] = useState('');
  const controller = useRef<AbortController | undefined>(undefined);
  const routesController = useRef<AbortController | undefined>(undefined);
  const routesRequested = useRef(false);
  const loadedRouteCacheRevision = useRef(routeCacheRevision);
  const query = new URLSearchParams({ tenant_external_id: tenant, limit: '10000' });
  const path = `/internal/v1/upstreams/${accountId}/models`;
  const errorText = (reason: unknown) => reason instanceof Error ? reason.message : t('common.requestFailed');
  const catalogFailure = (value: UpstreamModelCatalogResponse) => {
    const key = `providerCatalog.error.${value.error_code || 'unavailable'}`;
    const message = t(key);
    return message === key ? t('providerCatalog.error.unavailable') : message;
  };
  const parseCatalog = (value: unknown): UpstreamModelCatalogResponse => {
    if (!isUpstreamModelCatalog(value)) {
      throw new Error(t('providerCatalog.error.invalid_response'));
    }
    return value;
  };
  const parseSync = (value: unknown): UpstreamModelCatalogSyncResponse => {
    if (!value || typeof value !== 'object') throw new Error(t('providerCatalog.error.invalid_response'));
    const { price_sync: priceSync, ...catalogValue } = value as Record<string, unknown>;
    const catalog = parseCatalog(catalogValue);
    if (!priceSync || typeof priceSync !== 'object') {
      throw new Error(t('providerCatalog.error.invalid_response'));
    }
    const fields = priceSync as Record<string, unknown>;
    const failedSources = fields.failed_sources;
    if (typeof fields.status !== 'string' || !['ready', 'partial', 'error', 'skipped'].includes(fields.status)
      || fields.currency !== 'USD'
      || !['imported', 'preserved', 'unmatched', 'ambiguous'].every((field) => {
        const count = fields[field];
        return typeof count === 'number' && Number.isSafeInteger(count) && count >= 0;
      })
      || !Array.isArray(failedSources) || !failedSources.every((source) => typeof source === 'string')
      || (fields.error_code !== null && fields.error_code !== 'price_sync_failed')) {
      throw new Error(t('providerCatalog.error.invalid_response'));
    }
    return { ...catalog, price_sync: priceSync as UpstreamCatalogPriceSync };
  };

  useEffect(() => {
    controller.current?.abort();
    routesController.current?.abort();
    routesController.current = undefined;
    const read = new AbortController(); controller.current = read;
    setCatalog(undefined); setCatalogMessage(''); setCatalogError(''); setPriceSync(undefined);
    setBrowseFilter(''); setRoutes(undefined); setRoutesLoading(false); setRoutesError('');
    routesRequested.current = false; loadedRouteCacheRevision.current = routeCacheRevision;
    if (!token || !tenant) { setBusy(false); return () => read.abort(); }
    setBusy(true);
    void api<unknown>(`${path}?${query}`, token, { signal: read.signal }).then(parseCatalog)
      .then(value => { if (!read.signal.aborted) setCatalog(value); })
      .catch(reason => { if (!read.signal.aborted) setCatalogError(errorText(reason)); })
      .finally(() => { if (!read.signal.aborted) setBusy(false); });
    return () => { read.abort(); controller.current?.abort(); routesController.current?.abort(); };
  }, [accountId, tenant, token]);

  async function loadRoutes(force = false) {
    if (!token || !tenant || (!force && (routes || routesLoading))) return;
    routesRequested.current = true;
    routesController.current?.abort();
    const read = new AbortController(); routesController.current = read;
    setRoutesLoading(true); setRoutesError('');
    if (force) setRoutes(undefined);
    try {
      const loaded: ModelRouteView[] = [];
      let beforeCreatedAt: number | undefined;
      let beforeId: string | undefined;
      while (loaded.length < maximumRoutesLoaded) {
        const routesQuery = new URLSearchParams({ tenant_external_id: tenant, limit: String(routePageSize) });
        if (beforeCreatedAt !== undefined && beforeId) {
          routesQuery.set('before_created_at', String(beforeCreatedAt));
          routesQuery.set('before_id', beforeId);
        }
        const page = await apiRead<ModelRouteView[]>(`/internal/v1/model-routes?${routesQuery}`, token, { signal: read.signal });
        if (read.signal.aborted) return;
        loaded.push(...page);
        if (page.length < routePageSize) break;
        const last = page.at(-1);
        if (!last || !Number.isSafeInteger(last.created_at) || !last.id
          || (last.created_at === beforeCreatedAt && last.id === beforeId)) {
          throw new Error(t('providerCatalog.routesIncomplete'));
        }
        beforeCreatedAt = last.created_at; beforeId = last.id;
      }
      if (loaded.length >= maximumRoutesLoaded) throw new Error(t('providerCatalog.routesIncomplete'));
      if (!read.signal.aborted) setRoutes(loaded);
    } catch (reason) {
      if (!read.signal.aborted) { setRoutes(undefined); setRoutesError(errorText(reason)); }
    } finally {
      if (!read.signal.aborted) { setRoutesLoading(false); routesController.current = undefined; }
    }
  }

  useEffect(() => {
    if (loadedRouteCacheRevision.current === routeCacheRevision) return;
    loadedRouteCacheRevision.current = routeCacheRevision;
    if (routesRequested.current) void loadRoutes(true);
    // The revision is an explicit invalidation signal; the current scoped
    // route request is restarted even when a previous result is in flight.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [routeCacheRevision]);

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
  const filterText = browseFilter.trim().toLowerCase();
  const catalogModels = catalog?.models ?? [];
  const filteredModels = filterText ? catalogModels.filter((model) => model.id.toLowerCase().includes(filterText)) : catalogModels;
  return <section className="provider-model-catalog" aria-label={t('providerCatalog.title')} aria-busy={busy}>
    <div className="row-actions"><h4>{t('providerCatalog.title')}</h4><Button appearance="secondary" type="button" disabled={disabled || busy || !token || !tenant} onClick={() => void sync()}>{t('providerCatalog.sync')}</Button></div>
    <p className="muted">{busy && !catalog ? t('common.loading') : t(`providerCatalog.status.${status}`)}{catalog && <> · {t('providerCatalog.count', { count: formatNumber(catalog.models.length, locale) })}</>}{catalog?.last_success_at && <> · {new Date(catalog.last_success_at).toLocaleString(locale)}</>}</p>
    {catalogMessage && <p role="status">{catalogMessage}</p>}
    {visibleCatalogError && <p className="error" role="alert">{t('providerCatalog.modelsLabel')}: {visibleCatalogError}</p>}
    {priceSummary && <p role="status">{t('providerCatalog.pricesLabel')}: {priceSummary}</p>}
    {failedSources && <p className={priceSync?.status === 'partial' ? 'muted' : 'error'} role={priceSync?.status === 'partial' ? 'status' : 'alert'}>{t('providerCatalog.pricesLabel')}: {failedSources}</p>}
    {priceFailure && <p className="error" role="alert">{t('providerCatalog.pricesLabel')}: {priceFailure}</p>}
    {catalogModels.length > 0 && <Disclosure title={t('providerCatalog.viewCatalog', { count: formatNumber(catalogModels.length, locale) })} onOpenChange={(open) => { if (open) void loadRoutes(); }}>
      <Input value={browseFilter} placeholder={t('providerCatalog.searchCatalog')} aria-label={t('providerCatalog.searchCatalog')} onChange={(_, data) => setBrowseFilter(data.value)} />
      {routesLoading && <p className="muted" role="status">{t('providerCatalog.routesLoading')}</p>}
      {routesError && <div className="row-actions"><p className="error" role="alert">{routesError}</p><Button appearance="secondary" type="button" onClick={() => void loadRoutes(true)}>{t('providerCatalog.routesRetry')}</Button></div>}
      {filteredModels.length === 0 && <p className="muted">{t('providerCatalog.noCatalogMatches')}</p>}
      <ul className="provider-catalog-models">
        {filteredModels.slice(0, maximumCatalogModelsShown).map((model) => {
          const protocol = inferManagedRouteProtocol(model.protocol);
          const existing = routes && protocol ? findManagedRoute(routes, accountId, model.id, protocol) : undefined;
          return <li key={`${model.id} ${model.protocol}`}>
            <code>{model.id}</code><span className="muted">{model.protocol}</span>
            {!protocol && <span className="muted">{t('providerCatalog.routeProtocolUnsupported')}</span>}
            {onRouteAction && protocol && <Button appearance="secondary" type="button" disabled={routeActionDisabled || routesLoading || !routes || Boolean(routesError)}
              onClick={() => onRouteAction(existing ? { kind: 'view', routeId: existing.id } : { kind: 'create', model, protocol })}>{existing ? t('providerCatalog.viewRoute') : t('providerCatalog.addRoute')}</Button>}
          </li>;
        })}
      </ul>
      {filteredModels.length > maximumCatalogModelsShown && <p className="muted">{t('providerCatalog.catalogTruncated', { count: formatNumber(maximumCatalogModelsShown, locale) })}</p>}
    </Disclosure>}
    {disabledModels.length > 0 && <Disclosure title={t('providerCatalog.disabledModels', { count: formatNumber(disabledModels.length, locale) })}>
      <p className="muted">{t('providerCatalog.disabledModelsHint')}</p>
      <ul>{disabledModels.slice(0, maximumDisabledModelsShown).map((model) => <li key={`${model.id}\u0000${model.protocol}`}><code>{model.id}</code> · {model.protocol} · {t('common.disabled')}</li>)}</ul>
      {disabledModels.length > maximumDisabledModelsShown && <p className="muted">{t('providerCatalog.disabledModelsRemaining', { count: formatNumber(disabledModels.length - maximumDisabledModelsShown, locale) })}</p>}
    </Disclosure>}
  </section>;
}
