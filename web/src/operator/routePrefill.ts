import type { CatalogRouteAction, ManagedRouteProtocol } from './managedModelSync.js';

/** Session-scoped handoff between the provider card and the route workspace. */
const prefillKey = 'mtc-route-draft-prefill-v1';
const focusKey = 'mtc-route-focus-v1';

export interface RouteDraftPrefill {
  tenant: string;
  accountId: string;
  upstreamModel: string;
  publicModel: string;
  protocol: ManagedRouteProtocol;
}

function read(key: string): unknown {
  try {
    const raw = window.sessionStorage.getItem(key);
    return raw ? JSON.parse(raw) : undefined;
  } catch {
    return undefined;
  }
}

function remove(key: string): void {
  try {
    window.sessionStorage.removeItem(key);
  } catch {
    // Storage can be unavailable in hardened browsers.
  }
}

function write(key: string, value: unknown): boolean {
  try {
    window.sessionStorage.setItem(key, JSON.stringify(value));
    return true;
  } catch {
    // Storage can be unavailable in hardened browsers; navigation still works.
    return false;
  }
}

export function storeRouteDraftPrefill(prefill: RouteDraftPrefill): void {
  if (write(prefillKey, prefill)) remove(focusKey);
}

export function consumeRouteDraftPrefill(tenant: string): RouteDraftPrefill | undefined {
  const value = read(prefillKey);
  if (!value || typeof value !== 'object') return undefined;
  const prefill = value as Record<string, unknown>;
  if (typeof prefill.tenant !== 'string' || typeof prefill.accountId !== 'string'
    || typeof prefill.upstreamModel !== 'string' || typeof prefill.publicModel !== 'string'
    || (prefill.protocol !== 'openai' && prefill.protocol !== 'anthropic'
      && prefill.protocol !== 'openai-audio' && prefill.protocol !== 'generation')
    || !prefill.tenant || !prefill.accountId || !prefill.upstreamModel || !prefill.publicModel) return undefined;
  if (prefill.tenant !== tenant) return undefined;
  const result: RouteDraftPrefill = {
    tenant: prefill.tenant,
    accountId: prefill.accountId,
    upstreamModel: prefill.upstreamModel,
    publicModel: prefill.publicModel,
    protocol: prefill.protocol,
  };
  remove(prefillKey);
  return result;
}

export function storeRouteFocus(tenant: string, routeId: string): void {
  if (write(focusKey, { tenant, routeId })) remove(prefillKey);
}

export function consumeRouteFocus(tenant: string): string | undefined {
  const value = read(focusKey);
  if (!value || typeof value !== 'object') return undefined;
  const focus = value as Record<string, unknown>;
  if (typeof focus.tenant !== 'string' || typeof focus.routeId !== 'string'
    || !focus.tenant || !focus.routeId || focus.tenant !== tenant) return undefined;
  remove(focusKey);
  return focus.routeId;
}

export function storeCatalogRouteAction(tenant: string, accountId: string, action: CatalogRouteAction): void {
  if (action.kind === 'view') storeRouteFocus(tenant, action.routeId);
  else storeRouteDraftPrefill({
    tenant,
    accountId,
    upstreamModel: action.model.id,
    publicModel: action.model.id,
    protocol: action.protocol,
  });
}
