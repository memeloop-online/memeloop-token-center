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
] as ModelRouteView[];
const groups = [
  { id: 'group-d', name: 'Archive pool', member_ids: ['d'] },
  { id: 'group-a', name: 'Research pool', member_ids: ['a'] },
  { id: 'group-b', name: 'Production pool', member_ids: ['b'] },
  { id: 'group-c', name: 'Retired pool', member_ids: ['c'] },
];
globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
  const url = new URL(typeof input === 'string' ? input : input instanceof URL ? input.toString() : input.url, location.origin);
  if (init?.method && init.method !== 'GET') await (window as unknown as { recordModelPickerWrite?: (path: string) => Promise<void> }).recordModelPickerWrite?.(url.pathname);
  const values: Record<string, unknown> = {
    '/internal/v1/upstreams': accounts,
    '/internal/v1/model-routes': routes,
    '/internal/v1/provider-groups': groups,
    '/internal/v1/filter-presets': { named: [], recent: [] },
    '/internal/v1/filter-assistant/settings': null,
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
