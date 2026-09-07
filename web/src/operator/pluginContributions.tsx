import { useEffect, useState } from 'react';
import { ApiError, api } from '../api';
import { pluginRouteKey, type PluginRouteKey } from '../app/routes';
import type {
  PluginManifest,
  PluginOperatorUiContribution,
  PluginServiceDataResponse,
} from '../types';

/**
 * Browser-side policy boundary for operator plugins.
 *
 * The registry accepts only server-validated manifest data and maps it onto a
 * small set of local typed-data components. It intentionally has no dynamic
 * import, URL renderer, iframe, HTML parser, or script/style injection API.
 */
export interface RegisteredPluginContribution {
  pluginId: string;
  contribution: PluginOperatorUiContribution;
  route: PluginRouteKey | null;
}

export interface PluginNavigationItem {
  route: PluginRouteKey;
  label: string;
  icon: PluginOperatorUiContribution['icon'];
}

export interface PluginNavigationSection {
  id: string;
  label: string;
  items: PluginNavigationItem[];
}

export interface OperatorPluginRegistry {
  navigation: PluginNavigationSection[];
  pages: Map<PluginRouteKey, RegisteredPluginContribution>;
  overviewCards: RegisteredPluginContribution[];
}

const token = /^[a-z0-9-]{1,64}$/;
const coreCategories = new Set(['monitoring', 'traffic', 'identity', 'system']);
const supportedIcons = new Set<PluginOperatorUiContribution['icon']>(['activity', 'chart', 'database', 'heart', 'plug', 'shield']);

function safeLabel(value: unknown): value is string {
  return typeof value === 'string' && value.trim().length > 0 && value.length <= 120 && !/[\u0000-\u001f\u007f<>]/u.test(value);
}

function validContribution(value: PluginOperatorUiContribution): boolean {
  return token.test(value.id)
    && safeLabel(value.label)
    && token.test(value.data_endpoint)
    && value.renderer === 'typed_data_v1'
    && supportedIcons.has(value.icon);
}

export function registerOperatorPluginContributions(manifests: PluginManifest[]): OperatorPluginRegistry {
  const navigation = new Map<string, PluginNavigationSection>();
  const pages = new Map<PluginRouteKey, RegisteredPluginContribution>();
  const overviewCards: RegisteredPluginContribution[] = [];
  const sidebar: Array<RegisteredPluginContribution & { route: PluginRouteKey; category: NonNullable<PluginOperatorUiContribution['category']> }> = [];
  for (const manifest of manifests) {
    if (!token.test(manifest.id)) continue;
    const endpoints = new Set((manifest.contributions.service_data ?? []).map((endpoint) => endpoint.id).filter((id) => token.test(id)));
    for (const contribution of manifest.contributions.operator_ui ?? []) {
      if (!validContribution(contribution) || !endpoints.has(contribution.data_endpoint)) continue;
      if (contribution.slot === 'operator.overview.card') {
        if (contribution.route || contribution.category) continue;
        overviewCards.push({ pluginId: manifest.id, contribution, route: null });
        continue;
      }
      if (contribution.slot !== 'operator.sidebar.tab' || !token.test(contribution.route ?? '')) continue;
      const category = contribution.category;
      if (!category || !token.test(category.id)) continue;
      if (!coreCategories.has(category.id) && !safeLabel(category.label)) continue;
      const route = pluginRouteKey(manifest.id, contribution.route!);
      sidebar.push({ pluginId: manifest.id, contribution, route, category });
    }
  }

  // Treat a stale or malicious manifest list exactly like the service does:
  // do not let load order pick a winning route or new-category label.
  const routeCounts = new Map<string, number>();
  const categoryLabels = new Map<string, string>();
  const conflictingCategories = new Set<string>();
  for (const registered of sidebar) {
    const rawRoute = registered.contribution.route!;
    routeCounts.set(rawRoute, (routeCounts.get(rawRoute) ?? 0) + 1);
    if (coreCategories.has(registered.category.id)) continue;
    const label = registered.category.label!;
    const existing = categoryLabels.get(registered.category.id);
    if (existing !== undefined && existing !== label) conflictingCategories.add(registered.category.id);
    else categoryLabels.set(registered.category.id, label);
  }
  for (const registered of sidebar) {
    if (routeCounts.get(registered.contribution.route!) !== 1 || conflictingCategories.has(registered.category.id) || pages.has(registered.route)) continue;
    pages.set(registered.route, registered);
    const existing = navigation.get(registered.category.id);
    if (existing) {
      existing.items.push({ route: registered.route, label: registered.contribution.label, icon: registered.contribution.icon });
    } else {
      navigation.set(registered.category.id, {
        id: registered.category.id,
        label: coreCategories.has(registered.category.id) ? registered.category.id : registered.category.label!,
        items: [{ route: registered.route, label: registered.contribution.label, icon: registered.contribution.icon }],
      });
    }
  }
  return { navigation: [...navigation.values()], pages, overviewCards };
}

function serviceDataPath(pluginId: string, endpointId: string, tenant: string) {
  const query = tenant ? `?${new URLSearchParams({ tenant_external_id: tenant }).toString()}` : '';
  return `/internal/v1/plugins/${encodeURIComponent(pluginId)}/data/${encodeURIComponent(endpointId)}${query}`;
}

function renderJson(value: Record<string, unknown>, limit: number) {
  const encoded = JSON.stringify(value, null, 2);
  return encoded.length > limit ? `${encoded.slice(0, limit)}\n…` : encoded;
}

function TypedPluginData({ registered, token: credential, tenant, compact = false }: {
  registered: RegisteredPluginContribution;
  token: string;
  tenant: string;
  compact?: boolean;
}) {
  const [response, setResponse] = useState<PluginServiceDataResponse>();
  const [error, setError] = useState('');
  const endpoint = registered.contribution.data_endpoint;

  useEffect(() => {
    let active = true;
    setResponse(undefined); setError('');
    void api<PluginServiceDataResponse>(serviceDataPath(registered.pluginId, endpoint, tenant), credential)
      .then((value) => { if (active) setResponse(value); })
      .catch((reason: unknown) => {
        if (!active) return;
        setError(reason instanceof ApiError && reason.status === 403
          ? 'You do not have permission to view this plugin data.'
          : 'Plugin data is currently unavailable.');
      });
    return () => { active = false; };
  }, [credential, endpoint, registered.pluginId, tenant]);

  if (error) return <div className="notice error" role="alert">{error}</div>;
  if (!response) return <div className="empty">Loading plugin data…</div>;
  return <>
    {response.partial && <div className="notice" role="status">Showing degraded plugin data ({response.provenance.source.replaceAll('_', ' ')}).</div>}
    <pre className="plugin-typed-data" aria-label={`${registered.contribution.label} data`}>{renderJson(response.data, compact ? 1_500 : 12_000)}</pre>
    {!compact && <p className="muted">Source: {response.provenance.origin} · {new Date(response.provenance.fetched_at).toLocaleString()}</p>}
  </>;
}

export function PluginContributionPage({ registered, token, tenant }: {
  registered: RegisteredPluginContribution;
  token: string;
  tenant: string;
}) {
  return <article className="panel plugin-contribution-page">
    <div className="panel-title"><div><h2>{registered.contribution.label}</h2><p className="muted">Plugin-provided typed data rendered by Token Center.</p></div></div>
    <TypedPluginData registered={registered} token={token} tenant={tenant} />
  </article>;
}

export function PluginOverviewCards({ cards, token, tenant }: {
  cards: RegisteredPluginContribution[];
  token: string;
  tenant: string;
}) {
  if (cards.length === 0) return null;
  return <section className="operator-overview-plugin-cards" aria-label="Plugin contributions">
    {cards.map((registered) => <article className="panel" key={`${registered.pluginId}:${registered.contribution.id}`}>
      <div className="panel-title"><h2>{registered.contribution.label}</h2></div>
      <TypedPluginData registered={registered} token={token} tenant={tenant} compact />
    </article>)}
  </section>;
}
