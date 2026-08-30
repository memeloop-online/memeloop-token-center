export const selfPortalRoutes = [
  'overview',
  'requests',
  'sessions',
  'usage',
  'generations',
  'generate',
] as const;

export type SelfPortalRoute = typeof selfPortalRoutes[number];

export function isSelfPortalRoute(value: string | null | undefined): value is SelfPortalRoute {
  return selfPortalRoutes.includes(value as SelfPortalRoute);
}

export function selfPortalRouteFromSearch(search: string): SelfPortalRoute {
  const view = new URLSearchParams(search).get('view');
  return isSelfPortalRoute(view) ? view : 'overview';
}

export function selfPortalSearchForRoute(route: SelfPortalRoute): string {
  return route === 'overview' ? '' : `?view=${encodeURIComponent(route)}`;
}
