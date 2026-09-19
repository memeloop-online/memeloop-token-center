import type {
  ManagedModelSyncResponse, ManagedRoutePriceSync, ManagedRouteSyncCounts, ModelRouteView, UpstreamCatalogModel, UpstreamModelCatalogResponse,
} from '../types';

/** The sync-routes contract is strict; reject partial payloads instead of guessing. */
export class ManagedSyncResponseError extends Error {
  constructor() {
    super('invalid managed sync response');
    this.name = 'ManagedSyncResponseError';
  }
}

function isCatalog(value: unknown): value is UpstreamModelCatalogResponse {
  if (!value || typeof value !== 'object') return false;
  const catalog = value as Record<string, unknown>;
  return typeof catalog.account_id === 'string'
    && typeof catalog.status === 'string'
    && typeof catalog.credential_generation === 'number'
    && (catalog.last_attempt_at === null || typeof catalog.last_attempt_at === 'number')
    && (catalog.last_success_at === null || typeof catalog.last_success_at === 'number')
    && (catalog.expires_at === null || typeof catalog.expires_at === 'number')
    && (catalog.error_code === null || typeof catalog.error_code === 'string')
    && Array.isArray(catalog.models)
    && catalog.models.every((model) => model && typeof model.id === 'string' && typeof model.protocol === 'string')
    && Array.isArray(catalog.disabled_models)
    && catalog.disabled_models.every((model) => model && typeof model.id === 'string' && typeof model.protocol === 'string'
      && model.status === 'disabled' && typeof model.disabled_at === 'number' && model.reason === 'removed_from_upstream');
}

function isCounts(value: unknown): value is ManagedRouteSyncCounts {
  if (!value || typeof value !== 'object') return false;
  const counts = value as Record<string, unknown>;
  return ['added', 'disabled', 'restored', 'unchanged', 'skipped'].every((field) => {
    const count = counts[field];
    return typeof count === 'number' && Number.isInteger(count) && count >= 0;
  })
    && Array.isArray(counts.warnings) && counts.warnings.every((warning) => typeof warning === 'string');
}

function isPriceSync(value: unknown): value is ManagedRoutePriceSync {
  if (!value || typeof value !== 'object') return false;
  const price = value as Record<string, unknown>;
  return price.status === 'deferred'
    && price.currency === 'USD'
    && ['imported', 'preserved', 'unmatched', 'ambiguous'].every((field) => typeof price[field] === 'number')
    && Array.isArray(price.failed_sources) && price.failed_sources.every((source) => typeof source === 'string')
    && price.error_code === 'managed_route_price_sync_deferred';
}

export function parseManagedModelSync(value: unknown): ManagedModelSyncResponse {
  if (!value || typeof value !== 'object') throw new ManagedSyncResponseError();
  const response = value as Record<string, unknown>;
  if (!isCatalog(response.catalog) || !isCounts(response.routes) || !isPriceSync(response.price_sync)) {
    throw new ManagedSyncResponseError();
  }
  return { catalog: response.catalog, routes: response.routes, price_sync: response.price_sync };
}

/** Warnings mean reconciliation was skipped or partial even when the HTTP call succeeded. */
export function managedSyncTone(result: ManagedModelSyncResponse): 'success' | 'partial' {
  return result.routes.warnings.length > 0 ? 'partial' : 'success';
}

/** Route protocols the route form can express; catalog wildcard entries map to openai. */
export const managedRouteProtocols: readonly string[] = ['openai', 'anthropic', 'openai-audio', 'generation'];

export function inferManagedRouteProtocol(catalogProtocol: string): string {
  return managedRouteProtocols.includes(catalogProtocol) ? catalogProtocol : 'openai';
}

/** A managed route already covering this account/model/protocol must be viewed, not recreated. */
export function findManagedRoute(routes: ModelRouteView[], accountId: string, upstreamModel: string, protocol: string): ModelRouteView | undefined {
  return routes.find((route) => {
    const accountIds = route.upstream_account_ids ?? (route.upstream_account_id ? [route.upstream_account_id] : []);
    return accountIds.includes(accountId) && route.upstream_model === upstreamModel && route.protocol === protocol;
  });
}

/** Catalog rows either open the matching managed route or hand a prefilled draft to the route workspace. */
export type CatalogRouteAction =
  | { kind: 'create'; model: UpstreamCatalogModel; protocol: string }
  | { kind: 'view'; routeId: string };
