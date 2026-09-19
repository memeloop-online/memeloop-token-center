/** Session-scoped handoff between the provider card and the route workspace. */
const prefillKey = 'mtc-route-draft-prefill-v1';
const focusKey = 'mtc-route-focus-v1';

export interface RouteDraftPrefill {
  tenant: string;
  accountId: string;
  upstreamModel: string;
  publicModel: string;
  protocol: string;
}

function read(key: string): unknown {
  try {
    const raw = window.sessionStorage.getItem(key);
    if (raw) window.sessionStorage.removeItem(key);
    return raw ? JSON.parse(raw) : undefined;
  } catch {
    return undefined;
  }
}

function write(key: string, value: unknown): void {
  try {
    window.sessionStorage.setItem(key, JSON.stringify(value));
  } catch {
    // Storage can be unavailable in hardened browsers; navigation still works.
  }
}

export function storeRouteDraftPrefill(prefill: RouteDraftPrefill): void {
  write(prefillKey, prefill);
}

export function consumeRouteDraftPrefill(tenant: string): RouteDraftPrefill | undefined {
  const value = read(prefillKey);
  if (!value || typeof value !== 'object') return undefined;
  const prefill = value as Record<string, unknown>;
  if (typeof prefill.tenant !== 'string' || typeof prefill.accountId !== 'string'
    || typeof prefill.upstreamModel !== 'string' || typeof prefill.publicModel !== 'string'
    || typeof prefill.protocol !== 'string' || !prefill.accountId || !prefill.upstreamModel) return undefined;
  if (prefill.tenant !== tenant) return undefined;
  return {
    tenant: prefill.tenant,
    accountId: prefill.accountId,
    upstreamModel: prefill.upstreamModel,
    publicModel: prefill.publicModel,
    protocol: prefill.protocol,
  };
}

export function storeRouteFocus(tenant: string, routeId: string): void {
  write(focusKey, { tenant, routeId });
}

export function consumeRouteFocus(tenant: string): string | undefined {
  const value = read(focusKey);
  if (!value || typeof value !== 'object') return undefined;
  const focus = value as Record<string, unknown>;
  return focus.tenant === tenant && typeof focus.routeId === 'string' && focus.routeId ? focus.routeId : undefined;
}
