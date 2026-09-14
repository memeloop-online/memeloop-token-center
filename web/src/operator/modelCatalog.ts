import type { GroupView, ModelRouteView, UpstreamAccount } from '../types.js';

export interface ModelPickerProjectionSource {
  route_id: string;
  provider: { id: string; label: string; protocols: string[]; modalities: string[] };
  provider_groups: Array<{ id: string; label: string }>;
  account: { id: string; label: string };
  configuration_availability: { status: 'available' | 'unavailable'; reasons: string[] };
  catalog: { status: 'never_observed' | 'loading' | 'ready' | 'stale' | 'partial' | 'error'; model_listed: boolean };
  capabilities: { route_protocol: string; upstream_model: string; catalog_model_listed: boolean };
}

export interface ModelPickerProjectionItem {
  selection: { kind: 'route'; route_id: string };
  value: string;
  label: string;
  sources: ModelPickerProjectionSource[];
}

export interface ModelPickerProjectionPage {
  contract_version: 'model_picker_projection_v1';
  data: ModelPickerProjectionItem[];
  next_cursor: string | null;
}

export interface RouteModelOption {
  key: string; value: string; label: string; providerGroup?: string; provider: string; upstream: string; description: string;
  availability: 'available' | 'unavailable' | 'unknown';
  health: 'unknown';
  capabilities: string[];
  disabled: boolean;
}

export interface AssistantRouteCatalog {
  options: RouteModelOption[];
  unverifiedRoutes: Array<{ value: string; label: string }>;
}

/**
 * Admit a filter-assistant source only when the authoritative provider catalog
 * and the current model catalog jointly prove text generation. The projection
 * has no model-level modality, so a provider that declares any non-text
 * modality is ambiguous and must fail closed. Model names and custom aliases
 * are deliberately never treated as capability evidence.
 */
export function assistantRouteCatalog(items: ModelPickerProjectionItem[]): AssistantRouteCatalog {
  const options: RouteModelOption[] = [];
  const unverifiedRoutes: AssistantRouteCatalog['unverifiedRoutes'] = [];
  for (const item of items) {
    const verifiedSources = item.sources.filter((source) => {
      const protocol = source.capabilities.route_protocol;
      return (protocol === 'openai' || protocol === 'anthropic')
        && source.provider.protocols.includes(protocol)
        && source.provider.modalities.length === 1
        && source.provider.modalities[0] === 'text'
        && source.catalog.status === 'ready'
        && source.catalog.model_listed
        && source.capabilities.catalog_model_listed;
    });
    if (verifiedSources.length === 0) {
      unverifiedRoutes.push({ value: item.value, label: item.label });
      continue;
    }
    for (const source of verifiedSources) {
      const available = source.configuration_availability.status === 'available';
      options.push({
        key: `${source.route_id}:${source.account.id}`,
        value: item.value,
        label: item.label,
        providerGroup: source.provider_groups.map((group) => group.label).join(' · ') || undefined,
        provider: source.provider.label,
        upstream: source.account.label,
        description: source.capabilities.upstream_model,
        availability: available ? 'available' : 'unavailable',
        health: 'unknown',
        capabilities: [source.capabilities.route_protocol, 'text'],
        disabled: !available,
      });
    }
  }
  return { options, unverifiedRoutes };
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
