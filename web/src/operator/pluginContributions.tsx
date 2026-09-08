import { useEffect, useState } from 'react';
import { ApiError, api } from '../api.js';
import { pluginRouteKey, type PluginRouteKey } from '../app/routes.js';
import type {
  PluginManifest,
  PluginOperatorUiContribution,
  PluginServiceDataResponse,
} from '../types.js';

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
const supportedPresentations = new Set<NonNullable<PluginOperatorUiContribution['presentation']>>(['health_intelligence_v1']);
const healthSourceIds = new Set(['codexradar', 'deepswe', 'aixhan']);
const healthSourceStatuses = new Set(['ok', 'stale', 'error']);

function safeLabel(value: unknown): value is string {
  return typeof value === 'string' && value.trim().length > 0 && value.length <= 120 && !/[\u0000-\u001f\u007f<>]/u.test(value);
}

function validContribution(value: PluginOperatorUiContribution): boolean {
  return token.test(value.id)
    && safeLabel(value.label)
    && token.test(value.data_endpoint)
    && value.renderer === 'typed_data_v1'
    && supportedIcons.has(value.icon)
    && (value.presentation == null || supportedPresentations.has(value.presentation));
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

  // Keep the first validated category declaration as the owner of its label.
  // A later conflicting declaration is rejected on its own; it must not erase
  // the already-valid navigation section or replace its label.
  const routeCounts = new Map<string, number>();
  const categoryLabels = new Map<string, string>();
  const conflictingCategoryRoutes = new Set<PluginRouteKey>();
  for (const registered of sidebar) {
    const rawRoute = registered.contribution.route!;
    routeCounts.set(rawRoute, (routeCounts.get(rawRoute) ?? 0) + 1);
    if (coreCategories.has(registered.category.id)) continue;
    const label = registered.category.label!;
    const existing = categoryLabels.get(registered.category.id);
    if (existing !== undefined && existing !== label) conflictingCategoryRoutes.add(registered.route);
    else categoryLabels.set(registered.category.id, label);
  }
  for (const registered of sidebar) {
    if (routeCounts.get(registered.contribution.route!) !== 1 || conflictingCategoryRoutes.has(registered.route) || pages.has(registered.route)) continue;
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

type HealthSourceId = 'codexradar' | 'deepswe' | 'aixhan';
type HealthSourceStatus = 'ok' | 'stale' | 'error';

export interface HealthIntelligenceRow {
  title: string;
  value: string;
  detail: string | null;
}

export interface HealthIntelligenceSource {
  id: HealthSourceId;
  label: string;
  status: HealthSourceStatus;
  rows: HealthIntelligenceRow[];
}

export interface HealthIntelligenceSnapshot {
  generatedAt: string;
  sources: HealthIntelligenceSource[];
}

function record(value: unknown): Record<string, unknown> | null {
  return typeof value === 'object' && value !== null && !Array.isArray(value) ? value as Record<string, unknown> : null;
}

function text(value: unknown, limit = 120): string | null {
  return typeof value === 'string' && value.trim().length > 0 && value.length <= limit && !/[\u0000-\u001f\u007f]/u.test(value)
    ? value : null;
}

function numeric(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

function formatMetric(value: number, fractionDigits = 0): string {
  return new Intl.NumberFormat(undefined, { maximumFractionDigits: fractionDigits }).format(value);
}

function sourceRows(id: HealthSourceId, value: unknown): HealthIntelligenceRow[] | null {
  if (!Array.isArray(value) || value.length > 24) return null;
  const rows: HealthIntelligenceRow[] = [];
  for (const candidate of value.slice(0, 6)) {
    const row = record(candidate);
    if (!row) return null;
    if (id === 'codexradar') {
      const model = text(row.model);
      const effort = text(row.effort, 48);
      const iq = numeric(row.iq);
      if (!model || !effort || iq == null) return null;
      const samples = numeric(row.samples);
      rows.push({ title: `${model} · ${effort}`, value: `IQ ${formatMetric(iq, 1)}`, detail: samples == null ? null : `${formatMetric(samples)} samples` });
    } else if (id === 'deepswe') {
      const model = text(row.model);
      const effort = text(row.effort, 48);
      const passRate = numeric(row.passRate);
      if (!model || !effort || passRate == null || passRate < 0 || passRate > 1) return null;
      const steps = numeric(row.agentSteps);
      rows.push({ title: `${model} · ${effort}`, value: `${formatMetric(passRate * 100, 1)}%`, detail: steps == null ? null : `${formatMetric(steps)} steps` });
    } else {
      const name = text(row.name);
      const status = text(row.status, 32);
      if (!name || !status) return null;
      const model = text(row.model);
      const latency = numeric(row.latencyMs);
      const detail = [model, latency == null ? null : `${formatMetric(latency)} ms`].filter((item): item is string => item != null).join(' · ');
      rows.push({ title: name, value: status, detail: detail || null });
    }
  }
  return rows;
}

/**
 * Narrows the published health-intelligence snapshot into a small, core-owned
 * view model. Any data that does not meet the exact bounded shape falls back
 * to the generic typed-data renderer rather than being guessed or executed.
 */
export function healthIntelligenceSnapshot(value: Record<string, unknown>): HealthIntelligenceSnapshot | null {
  if (value.schemaVersion !== 1 || !Array.isArray(value.sources) || value.sources.length !== 3) return null;
  const generatedAt = text(value.generatedAt, 64);
  if (!generatedAt) return null;
  const seen = new Set<string>();
  const sources: HealthIntelligenceSource[] = [];
  for (const candidate of value.sources) {
    const source = record(candidate);
    if (!source) return null;
    const id = text(source.id, 32);
    const label = text(source.label);
    const status = text(source.status, 16);
    if (!id || !healthSourceIds.has(id) || seen.has(id) || !label || !status || !healthSourceStatuses.has(status)) return null;
    const rows = sourceRows(id as HealthSourceId, source.rows);
    if (!rows) return null;
    seen.add(id);
    sources.push({ id: id as HealthSourceId, label, status: status as HealthSourceStatus, rows });
  }
  return seen.size === healthSourceIds.size
    ? { generatedAt, sources }
    : null;
}

function HealthIntelligencePanel({ snapshot, compact }: { snapshot: HealthIntelligenceSnapshot; compact: boolean }) {
  const rowLimit = compact ? 2 : 6;
  return <section className="operator-overview-plugin-cards" aria-label="Health and intelligence">
    {snapshot.sources.map((source) => <section className="managed-resource" key={source.id}>
      <header className="managed-resource-header">
        <h3>{source.label}</h3>
        <span className={`status ${source.status === 'ok' ? 'ok' : source.status === 'stale' ? 'pending' : 'bad'}`}>{source.status}</span>
      </header>
      <div className="account-list">
        {source.rows.length === 0 && <p className="muted">No current signals.</p>}
        {source.rows.slice(0, rowLimit).map((row) => <div className="account" key={`${source.id}:${row.title}`}>
          <div className="account-main"><b>{row.title}</b>{row.detail && <span>{row.detail}</span>}</div>
          <span className="pill">{row.value}</span>
        </div>)}
      </div>
    </section>)}
    {!compact && <p className="muted">Snapshot generated {new Date(snapshot.generatedAt).toLocaleString()}.</p>}
  </section>;
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
  const snapshot = registered.contribution.presentation === 'health_intelligence_v1'
    ? healthIntelligenceSnapshot(response.data)
    : null;
  return <>
    {response.partial && <div className="notice" role="status">Showing degraded plugin data ({response.provenance.source.replaceAll('_', ' ')}).</div>}
    {snapshot
      ? <HealthIntelligencePanel snapshot={snapshot} compact={compact} />
      : <pre className="plugin-typed-data" aria-label={`${registered.contribution.label} data`}>{renderJson(response.data, compact ? 1_500 : 12_000)}</pre>}
    {!compact && <p className="muted">Source: {response.provenance.origin} · {new Date(response.provenance.fetched_at).toLocaleString()}</p>}
  </>;
}

export function PluginContributionPage({ registered, token, tenant }: {
  registered: RegisteredPluginContribution;
  token: string;
  tenant: string;
}) {
  return <article className="panel plugin-contribution-page">
    <div className="panel-title"><div><h2>{registered.contribution.label}</h2><p className="muted">Data from the installed extension.</p></div></div>
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
