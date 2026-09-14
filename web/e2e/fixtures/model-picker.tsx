import { createRoot } from 'react-dom/client';
import { useState } from 'react';
import { I18nProvider } from '../../src/i18n';
import { TypedFilterBuilder } from '../../src/operator/TypedFilterBuilder';
import { SystemSettingsPage } from '../../src/operator/pages/SystemSettingsPage';
import type { ModelRouteView, TypedFilterAst, UpstreamAccount } from '../../src/types';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/operator.css';

const accounts = [
  { id: 'd', name: 'Archived account', driver: 'provider-0', status: 'disabled' },
  { id: 'a', name: 'Research account', driver: 'provider-a', status: 'active' },
  { id: 'b', name: 'Production account', driver: 'provider-b', status: 'active' },
  { id: 'c', name: 'Retired account', driver: 'provider-c', status: 'disabled' },
] as UpstreamAccount[];
const routes = [
  { id: 'route-d', public_model: 'archived-model', upstream_model: 'native-d', protocol: 'openai', upstream_account_ids: ['d'], enabled: true },
  { id: 'route-a', public_model: 'research-model', upstream_model: 'native-a', protocol: 'openai', upstream_account_ids: ['a'], enabled: true },
  { id: 'route-b', public_model: 'production-model', upstream_model: 'native-b', protocol: 'anthropic', upstream_account_ids: ['b'], enabled: true },
  { id: 'route-c', public_model: 'retired-model', upstream_model: 'native-c', protocol: 'openai', upstream_account_ids: ['c'], enabled: true },
  { id: 'route-image', public_model: 'image-only', upstream_model: 'native-image', protocol: 'openai-image', upstream_account_ids: ['a'], enabled: true },
  { id: 'route-video', public_model: 'video-only', upstream_model: 'native-video', protocol: 'generation', upstream_account_ids: ['a'], enabled: true },
  { id: 'route-embedding', public_model: 'embedding-only', upstream_model: 'qwen3.7-text-embedding', protocol: 'openai', upstream_account_ids: ['a'], enabled: true },
] as ModelRouteView[];
const groups = [
  { id: 'group-d', name: 'Archive pool', member_ids: ['d'] },
  { id: 'group-a', name: 'Research pool', member_ids: ['a'] },
  { id: 'group-b', name: 'Production pool', member_ids: ['b'] },
  { id: 'group-c', name: 'Retired pool', member_ids: ['c'] },
];
const source = ({ routeId, accountId, accountLabel, providerId, providerLabel, protocol = 'openai', modalities = ['text'], available = true, status = 'ready', listed = true, upstreamModel }: {
  routeId: string; accountId: string; accountLabel: string; providerId: string; providerLabel: string; protocol?: string; modalities?: string[]; available?: boolean; status?: string; listed?: boolean; upstreamModel: string;
}) => ({
  route_id: routeId,
  provider: { id: providerId, label: providerLabel, protocols: [protocol], modalities },
  provider_groups: [{ id: `group-${accountId}`, label: groups.find((group) => group.id === `group-${accountId}`)?.name ?? 'Other pool' }],
  account: { id: accountId, label: accountLabel },
  configuration_availability: { status: available ? 'available' : 'unavailable', reasons: available ? [] : ['account_inactive'] },
  catalog: { status, model_listed: listed },
  capabilities: { route_protocol: protocol, upstream_model: upstreamModel, catalog_model_listed: listed },
  passive_health: { status: 'unknown' },
});
const projection = {
  contract_version: 'model_picker_projection_v1', generated_at: Date.now(), next_cursor: null,
  data: [
    { selection: { kind: 'route', route_id: 'route-d' }, value: 'route-d', label: 'archived-model', sources: [source({ routeId: 'route-d', accountId: 'd', accountLabel: 'Archived account', providerId: 'provider-0', providerLabel: 'provider-0', available: false, upstreamModel: 'native-d' })] },
    { selection: { kind: 'route', route_id: 'route-a' }, value: 'route-a', label: 'research-model', sources: [source({ routeId: 'route-a', accountId: 'a', accountLabel: 'Research account', providerId: 'provider-a', providerLabel: 'provider-a', upstreamModel: 'native-a' })] },
    { selection: { kind: 'route', route_id: 'route-b' }, value: 'route-b', label: 'production-model', sources: [source({ routeId: 'route-b', accountId: 'b', accountLabel: 'Production account', providerId: 'provider-b', providerLabel: 'provider-b', protocol: 'anthropic', upstreamModel: 'native-b' })] },
    { selection: { kind: 'route', route_id: 'route-c' }, value: 'route-c', label: 'retired-model', sources: [source({ routeId: 'route-c', accountId: 'c', accountLabel: 'Retired account', providerId: 'provider-c', providerLabel: 'provider-c', available: false, upstreamModel: 'native-c' })] },
    { selection: { kind: 'route', route_id: 'route-image' }, value: 'route-image', label: 'image-only', sources: [source({ routeId: 'route-image', accountId: 'a', accountLabel: 'Research account', providerId: 'provider-a', providerLabel: 'provider-a', protocol: 'openai-image', modalities: ['image'], upstreamModel: 'native-image' })] },
    { selection: { kind: 'route', route_id: 'route-video' }, value: 'route-video', label: 'video-only', sources: [source({ routeId: 'route-video', accountId: 'a', accountLabel: 'Research account', providerId: 'provider-a', providerLabel: 'provider-a', protocol: 'generation', modalities: ['video'], upstreamModel: 'native-video' })] },
    { selection: { kind: 'route', route_id: 'route-embedding' }, value: 'route-embedding', label: 'embedding-model', sources: [source({ routeId: 'route-embedding', accountId: 'a', accountLabel: 'Research account', providerId: 'generic-http', providerLabel: 'Generic HTTP', modalities: ['text', 'embedding', 'image'], upstreamModel: 'embeddinggemma-300m' })] },
    { selection: { kind: 'route', route_id: 'route-moderation' }, value: 'route-moderation', label: 'moderation-model', sources: [source({ routeId: 'route-moderation', accountId: 'a', accountLabel: 'Research account', providerId: 'generic-http', providerLabel: 'Generic HTTP', modalities: ['text', 'embedding', 'image'], upstreamModel: 'omni-moderation-latest' })] },
    { selection: { kind: 'route', route_id: 'route-legal-alias' }, value: 'route-legal-alias', label: 'image-analysis-assistant', sources: [source({ routeId: 'route-legal-alias', accountId: 'a', accountLabel: 'Research account', providerId: 'provider-a', providerLabel: 'provider-a', upstreamModel: 'legal-text-alias' })] },
    { selection: { kind: 'route', route_id: 'route-custom' }, value: 'route-custom', label: 'friendly-custom-chat', sources: [source({ routeId: 'route-custom', accountId: 'a', accountLabel: 'Research account', providerId: 'provider-a', providerLabel: 'provider-a', status: 'never_observed', listed: false, upstreamModel: 'friendly-custom-chat' })] },
  ],
};
globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
  const url = new URL(typeof input === 'string' ? input : input instanceof URL ? input.toString() : input.url, location.origin);
  if (init?.method && init.method !== 'GET') await (window as unknown as { recordModelPickerWrite?: (path: string) => Promise<void> }).recordModelPickerWrite?.(url.pathname);
  const values: Record<string, unknown> = {
    '/internal/v1/upstreams': accounts,
    '/internal/v1/model-routes': routes,
    '/internal/v1/model-picker-options': projection,
    '/internal/v1/provider-groups': groups,
    '/internal/v1/filter-presets': { named: [], recent: [] },
    '/internal/v1/filter-assistant/settings': { model_route_id: 'route-custom', updated_at: Date.now() },
  };
  return new Response(JSON.stringify(values[url.pathname] ?? {}), { status: Object.hasOwn(values, url.pathname) ? 200 : 404, headers: { 'Content-Type': 'application/json' } });
};
function Fixture() {
  const [ast, setAst] = useState<TypedFilterAst>({ logical_operator: 'and', conditions: [] });
  const [outside, setOutside] = useState(0);
  return <main style={{ padding: 12 }}>
    <button type="button" data-outside onClick={() => setOutside((value) => value + 1)}>Outside {outside}</button>
    <TypedFilterBuilder ast={ast} onApply={setAst} onClear={() => setAst({ logical_operator: 'and', conditions: [] })} token="fixture" tenant="tenant" scope="requests" upstreams={accounts} />
    <output data-filter-model>{ast.conditions.find((condition) => condition.field === 'model')?.value.value}</output>
    <SystemSettingsPage token="fixture" tenant="tenant" />
  </main>;
}
createRoot(document.getElementById('root')!).render(<I18nProvider><Fixture /></I18nProvider>);
