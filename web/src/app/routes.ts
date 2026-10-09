import { readProxyGroupNavigationContext, withProxyGroupNavigationContext, type ProxyGroupNavigationContext } from './proxyGroupNavigation.js';

export const portalRouteKeys = [
  'overview',
  'requests',
  'sessions',
  'usage',
  'generations',
  'generate',
] as const;

export const operatorRouteKeys = [
  'overview',
  'requests',
  'sessions',
  'usage',
  'generations',
  'providers',
  'proxy-groups',
  'routes',
  'pricing',
  'tenants',
  'credentials',
  'service-credentials',
  'plugins',
  'system-settings',
] as const;

export type PortalRouteKey = (typeof portalRouteKeys)[number];
export type OperatorRouteKey = (typeof operatorRouteKeys)[number];
/** Opaque, namespaced route generated only from a loaded plugin manifest. */
export type PluginRouteKey = `plugin--${string}--${string}`;
export type AppSurface = 'portal' | 'operator';
export type AppRouteKey = PortalRouteKey | OperatorRouteKey | PluginRouteKey;

export interface AppLocation {
  surface: AppSurface;
  route: AppRouteKey;
  context?: ProxyGroupNavigationContext;
}

const routeSets: Record<AppSurface, ReadonlySet<string>> = {
  portal: new Set(portalRouteKeys),
  operator: new Set(operatorRouteKeys),
};

export const defaultRoutes = {
  portal: 'overview',
  operator: 'overview',
} as const satisfies Record<AppSurface, AppRouteKey>;

const pluginRoutePattern = /^plugin--[a-z0-9-]{1,64}--[a-z0-9-]{1,64}$/;

export function pluginRouteKey(pluginId: string, route: string): PluginRouteKey {
  if (!/^[a-z0-9-]{1,64}$/.test(pluginId) || !/^[a-z0-9-]{1,64}$/.test(route)) {
    throw new Error('plugin route components must be bounded lowercase tokens');
  }
  return `plugin--${pluginId}--${route}` as PluginRouteKey;
}

export function isPluginRouteKey(value: string): value is PluginRouteKey {
  return pluginRoutePattern.test(value);
}

export function surfaceFromPathname(pathname: string): AppSurface {
  return pathname === '/operator' || pathname.startsWith('/operator/') ? 'operator' : 'portal';
}

export function readAppLocation(url: Pick<URL, 'pathname' | 'searchParams'>): AppLocation {
  const surface = surfaceFromPathname(url.pathname);
  const candidate = url.searchParams.get('view');
  const route = candidate && (routeSets[surface].has(candidate) || (surface === 'operator' && isPluginRouteKey(candidate)))
    ? candidate as AppRouteKey
    : defaultRoutes[surface];
  const context = surface === 'operator' && route === 'proxy-groups' ? readProxyGroupNavigationContext(url.searchParams.toString()) : {};
  return { surface, route, ...(context.accountId || context.tenant ? { context } : {}) };
}

/**
 * The Rust server currently exposes exact `/portal` and `/operator` document
 * routes. A `view` query keeps refresh and deep-link behavior real without
 * requiring a catch-all route or leaking any credential into browser history.
 */
export function appHref(surface: AppSurface, route: AppRouteKey, context: ProxyGroupNavigationContext = {}): string {
  if (!routeSets[surface].has(route) && !(surface === 'operator' && isPluginRouteKey(route))) throw new Error(`${route} is not a ${surface} route`);
  return withProxyGroupNavigationContext(`/${surface}?${new URLSearchParams({ view: route }).toString()}`, context);
}

export function isSameAppLocation(left: AppLocation, right: AppLocation): boolean {
  return left.surface === right.surface && left.route === right.route
    && left.context?.accountId === right.context?.accountId && left.context?.tenant === right.context?.tenant;
}
