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
  'routes',
  'pricing',
  'credentials',
  'service-credentials',
  'plugins',
] as const;

export type PortalRouteKey = (typeof portalRouteKeys)[number];
export type OperatorRouteKey = (typeof operatorRouteKeys)[number];
export type AppSurface = 'portal' | 'operator';
export type AppRouteKey = PortalRouteKey | OperatorRouteKey;

export interface AppLocation {
  surface: AppSurface;
  route: AppRouteKey;
}

const routeSets: Record<AppSurface, ReadonlySet<string>> = {
  portal: new Set(portalRouteKeys),
  operator: new Set(operatorRouteKeys),
};

export const defaultRoutes = {
  portal: 'overview',
  operator: 'overview',
} as const satisfies Record<AppSurface, AppRouteKey>;

export function surfaceFromPathname(pathname: string): AppSurface {
  return pathname === '/operator' || pathname.startsWith('/operator/') ? 'operator' : 'portal';
}

export function readAppLocation(url: Pick<URL, 'pathname' | 'searchParams'>): AppLocation {
  const surface = surfaceFromPathname(url.pathname);
  const candidate = url.searchParams.get('view');
  const route = candidate && routeSets[surface].has(candidate)
    ? candidate as AppRouteKey
    : defaultRoutes[surface];
  return { surface, route };
}

/**
 * The Rust server currently exposes exact `/portal` and `/operator` document
 * routes. A `view` query keeps refresh and deep-link behavior real without
 * requiring a catch-all route or leaking any credential into browser history.
 */
export function appHref(surface: AppSurface, route: AppRouteKey): string {
  if (!routeSets[surface].has(route)) throw new Error(`${route} is not a ${surface} route`);
  return `/${surface}?${new URLSearchParams({ view: route }).toString()}`;
}

export function isSameAppLocation(left: AppLocation, right: AppLocation): boolean {
  return left.surface === right.surface && left.route === right.route;
}
