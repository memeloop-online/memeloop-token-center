import type {
  ManagedModelSyncResponse, ManagedRoutePriceSync, ManagedRouteSyncCounts, ModelRouteView, UpstreamCatalogModel, UpstreamModelCatalogResponse,
} from '../types.js';

const catalogStatuses = new Set(['unknown', 'syncing', 'ready', 'stale', 'error']);
const catalogErrorCodes = new Set([
  'unsupported', 'destination_invalid', 'credential_invalid', 'connection_failed', 'authentication_failed',
  'rate_limited', 'upstream_unavailable', 'redirect_rejected', 'response_too_large', 'invalid_response',
  'codex_no_trusted_models',
]);
const priceSyncStatuses = new Set(['ready', 'partial', 'error', 'skipped']);
const reservationBoundSources = new Set(['mtc_context_window_bound', 'administrator_override']);

function isInteger(value: unknown, minimum?: number): value is number {
  return typeof value === 'number' && Number.isSafeInteger(value) && (minimum === undefined || value >= minimum);
}

function isNullableInteger(value: unknown, minimum?: number): value is number | null {
  return value === null || isInteger(value, minimum);
}

function hasOnlyKeys(value: Record<string, unknown>, keys: readonly string[]): boolean {
  const allowed = new Set(keys);
  return Object.keys(value).every((key) => allowed.has(key));
}

function isCatalogModel(value: unknown): value is UpstreamCatalogModel {
  if (!value || typeof value !== 'object') return false;
  const model = value as Record<string, unknown>;
  return hasOnlyKeys(model, ['id', 'protocol', 'context_window', 'reservation_token_bound', 'reservation_bound_source'])
    && typeof model.id === 'string' && model.id.length > 0 && model.id.length <= 500
    && typeof model.protocol === 'string' && model.protocol.length > 0 && model.protocol.length <= 64
    && isNullableInteger(model.context_window, 1)
    && isNullableInteger(model.reservation_token_bound, 1)
    && (model.reservation_bound_source === null
      || (typeof model.reservation_bound_source === 'string' && reservationBoundSources.has(model.reservation_bound_source)));
}

/** The sync-routes contract is strict; reject partial payloads instead of guessing. */
export class ManagedSyncResponseError extends Error {
  constructor() {
    super('invalid managed sync response');
    this.name = 'ManagedSyncResponseError';
  }
}

export function isUpstreamModelCatalog(value: unknown): value is UpstreamModelCatalogResponse {
  if (!value || typeof value !== 'object') return false;
  const catalog = value as Record<string, unknown>;
  return hasOnlyKeys(catalog, [
    'account_id', 'status', 'credential_generation', 'last_attempt_at', 'last_success_at', 'expires_at',
    'error_code', 'models', 'disabled_models',
  ])
    && typeof catalog.account_id === 'string'
    && typeof catalog.status === 'string' && catalogStatuses.has(catalog.status)
    && isInteger(catalog.credential_generation, 1)
    && isNullableInteger(catalog.last_attempt_at)
    && isNullableInteger(catalog.last_success_at)
    && isNullableInteger(catalog.expires_at)
    && (catalog.error_code === null || (typeof catalog.error_code === 'string' && catalogErrorCodes.has(catalog.error_code)))
    && Array.isArray(catalog.models) && catalog.models.length <= 10_000 && catalog.models.every(isCatalogModel)
    && Array.isArray(catalog.disabled_models) && catalog.disabled_models.length <= 10_000
    && catalog.disabled_models.every((value) => {
      if (!value || typeof value !== 'object') return false;
      const model = value as Record<string, unknown>;
      return hasOnlyKeys(model, ['id', 'protocol', 'status', 'disabled_at', 'reason'])
        && typeof model.id === 'string' && model.id.length > 0 && model.id.length <= 500
        && typeof model.protocol === 'string' && model.protocol.length > 0 && model.protocol.length <= 64
        && model.status === 'disabled' && isInteger(model.disabled_at) && model.reason === 'removed_from_upstream';
    });
}

function isCounts(value: unknown): value is ManagedRouteSyncCounts {
  if (!value || typeof value !== 'object') return false;
  const counts = value as Record<string, unknown>;
  if (!hasOnlyKeys(counts, ['added', 'disabled', 'restored', 'unchanged', 'skipped', 'warnings'])) return false;
  return ['added', 'disabled', 'restored', 'unchanged', 'skipped'].every((field) => {
    const count = counts[field];
    return isInteger(count, 0);
  })
    && Array.isArray(counts.warnings) && counts.warnings.every((warning) => typeof warning === 'string');
}

function isPriceSync(value: unknown): value is ManagedRoutePriceSync {
  if (!value || typeof value !== 'object') return false;
  const price = value as Record<string, unknown>;
  if (!hasOnlyKeys(price, ['status', 'currency', 'imported', 'preserved', 'unmatched', 'ambiguous', 'failed_sources', 'error_code'])
    || price.currency !== 'USD'
    || !['imported', 'preserved', 'unmatched', 'ambiguous'].every((field) => isInteger(price[field], 0))
    || !Array.isArray(price.failed_sources)
    || !price.failed_sources.every((source) => typeof source === 'string' && source.length > 0)) {
    return false;
  }
  if (price.status === 'deferred') {
    return ['imported', 'preserved', 'unmatched', 'ambiguous'].every((field) => price[field] === 0)
      && price.failed_sources.length === 0
      && price.error_code === 'managed_route_price_sync_deferred';
  }
  if (typeof price.status !== 'string' || !priceSyncStatuses.has(price.status)) return false;
  return price.status === 'error'
    ? price.error_code === 'price_sync_failed'
    : price.error_code === null;
}

export function parseManagedModelSync(value: unknown): ManagedModelSyncResponse {
  if (!value || typeof value !== 'object') throw new ManagedSyncResponseError();
  const response = value as Record<string, unknown>;
  if (!hasOnlyKeys(response, ['catalog', 'routes', 'price_sync'])
    || !isUpstreamModelCatalog(response.catalog) || !isCounts(response.routes) || !isPriceSync(response.price_sync)) {
    throw new ManagedSyncResponseError();
  }
  return { catalog: response.catalog, routes: response.routes, price_sync: response.price_sync };
}

/** Warnings mean reconciliation was skipped or partial even when the HTTP call succeeded. */
export function managedSyncTone(result: ManagedModelSyncResponse): 'success' | 'partial' {
  return result.routes.warnings.length > 0
    || result.price_sync.status === 'partial'
    || result.price_sync.status === 'error'
    ? 'partial' : 'success';
}

/** Route protocols the route form can express; only catalog wildcard entries map to openai. */
export const managedRouteProtocols = ['openai', 'anthropic', 'openai-audio', 'generation'] as const;
export type ManagedRouteProtocol = (typeof managedRouteProtocols)[number];

export function inferManagedRouteProtocol(catalogProtocol: string): ManagedRouteProtocol | undefined {
  if (catalogProtocol === 'any') return 'openai';
  return (managedRouteProtocols as readonly string[]).includes(catalogProtocol)
    ? catalogProtocol as ManagedRouteProtocol
    : undefined;
}

/** A managed route already covering this account/model/protocol must be viewed, not recreated. */
export function findManagedRoute(routes: ModelRouteView[], accountId: string, upstreamModel: string, protocol: string): ModelRouteView | undefined {
  return routes.find((route) => {
    const accountIds = route.candidate_upstream_account_ids
      ?? route.upstream_account_ids
      ?? (route.upstream_account_id ? [route.upstream_account_id] : []);
    return accountIds.includes(accountId) && route.upstream_model === upstreamModel && route.protocol === protocol;
  });
}

/** Catalog rows either open the matching managed route or hand a prefilled draft to the route workspace. */
export type CatalogRouteAction =
  | { kind: 'create'; model: UpstreamCatalogModel; protocol: ManagedRouteProtocol }
  | { kind: 'view'; routeId: string };
