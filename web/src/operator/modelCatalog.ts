import type { GroupView, ModelRouteView, UpstreamAccount } from '../types.js';

export interface RouteModelOption {
  key: string; value: string; label: string; provider: string; upstream: string; description: string;
}

/** Preserve recorded public-model/route identity; never infer providers from model names. */
export function routeModelOptions(routes: ModelRouteView[], accounts: UpstreamAccount[], groups: GroupView[], unknown: string, valueKind: 'model' | 'route' = 'model'): RouteModelOption[] {
  return routes.flatMap((route) => {
    const included = groups.filter((group) => route.included_provider_group_ids?.includes(group.id)).flatMap((group) => group.member_ids);
    const excluded = new Set(groups.filter((group) => route.excluded_provider_group_ids?.includes(group.id)).flatMap((group) => group.member_ids));
    const ids = [...new Set([...(route.upstream_account_ids ?? []), ...(route.upstream_account_id ? [route.upstream_account_id] : []), ...included])].filter((id) => !excluded.has(id));
    return (ids.length ? ids : ['']).map((id) => {
      const account = accounts.find((candidate) => candidate.id === id);
      return {
        key: `${route.id}:${id}`, value: valueKind === 'route' ? route.id : route.public_model,
        label: route.public_model, provider: account?.driver || unknown,
        upstream: account?.name || unknown, description: `${route.upstream_model} · ${route.protocol}`,
      };
    });
  }).filter((option) => option.label.trim());
}
