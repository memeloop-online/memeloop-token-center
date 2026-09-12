import type { GroupView, ModelRouteView, UpstreamAccount } from '../types.js';

/** Only conversational transports can produce the assistant's text output.
 * Keep this separate from traffic filtering, which must retain every protocol.
 * Unknown transports need an explicit capability contract before opting in.
 */
export function filterAssistantRoutes(routes: ModelRouteView[]): ModelRouteView[] {
  return routes.filter((route) => route.enabled && (route.protocol === 'openai' || route.protocol === 'anthropic'));
}

export interface RouteModelOption {
  key: string; value: string; label: string; providerGroup?: string; provider: string; upstream: string; description: string;
  availability: 'available' | 'unavailable' | 'unknown';
  health: 'unknown';
  capabilities: string[];
  disabled: boolean;
}

/** Preserve recorded public-model/route identity; never infer providers from model names. */
export function routeModelOptions(routes: ModelRouteView[], accounts: UpstreamAccount[], groups: GroupView[], unknown: string, valueKind: 'model' | 'route' = 'model'): RouteModelOption[] {
  return routes.flatMap((route) => {
    const included = groups.filter((group) => route.included_provider_group_ids?.includes(group.id)).flatMap((group) => group.member_ids);
    const excluded = new Set(groups.filter((group) => route.excluded_provider_group_ids?.includes(group.id)).flatMap((group) => group.member_ids));
    const ids = [...new Set([...(route.upstream_account_ids ?? []), ...(route.upstream_account_id ? [route.upstream_account_id] : []), ...included])].filter((id) => !excluded.has(id));
    return (ids.length ? ids : ['']).map((id) => {
      const account = accounts.find((candidate) => candidate.id === id);
      const memberships = groups.filter((group) => group.member_ids.includes(id));
      const credentialExpired = account?.credential_expires_at !== null
        && account?.credential_expires_at !== undefined
        && account.credential_expires_at <= Date.now();
      const available = Boolean(account && account.status === 'active' && !credentialExpired);
      const availability: RouteModelOption['availability'] = account ? available ? 'available' : 'unavailable' : 'unknown';
      return {
        key: `${route.id}:${id}`, value: valueKind === 'route' ? route.id : route.public_model,
        label: route.public_model, providerGroup: memberships.map((group) => group.name).join(' · ') || undefined,
        provider: account?.driver || unknown, upstream: account?.name || unknown, description: route.upstream_model,
        availability,
        // The route/settings endpoints contain configuration state only. Do
        // not imply that selecting this field performed a health probe.
        health: 'unknown' as const, capabilities: [route.protocol], disabled: !available,
      };
    });
  }).filter((option) => option.label.trim());
}
