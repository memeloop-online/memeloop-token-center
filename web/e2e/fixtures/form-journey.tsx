import { createRoot } from 'react-dom/client';
import { useState } from 'react';
import { I18nProvider } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system';
import { ProvidersPage, RoutesPage } from '../../src/operator/pages/ManagementPages';
import { AppShell } from '../../src/app/AppShell';
import type { AppRouteKey } from '../../src/app/routes';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/operator.css';
import '../../src/app-shell.css';

import type {} from '../support/form-journey-globals';
window.formJourneyReads = []; window.formJourneyWrites = 0;
window.failNextFormWrite = false;
const workflows = new URLSearchParams(location.search).has('workflows');
const existingRoute = { id: 'route-existing', tenant_external_id: 'fixture', public_model: 'research-model', upstream_model: 'fixture-model', protocol: 'openai', upstream_account_ids: ['account-native'], enabled: true, priority: 0, grant_revision: 1, created_at: 1, updated_at: 1 };
let routeRows = [existingRoute];
const account = { id: 'account-native', tenant_external_id: 'fixture', name: '研发订阅', driver: 'openai-codex', auth_kind: 'oauth', connection_method: 'oauth', status: 'active', config: { base_url: 'https://chatgpt.com/backend-api/codex' }, has_proxy: true, proxy_scheme: 'socks5h', proxy_remote_dns: true, can_update_transport_proxy: true, credential_generation: 1, route_count: 1, updated_at: 1 };
window.fetch = async (input, init) => {
  const path = new URL(typeof input === 'string' ? input : input instanceof URL ? input.href : input.url, location.origin).pathname;
  const method = init?.method ?? (input instanceof Request ? input.method : 'GET');
  if (method !== 'GET') {
    window.formJourneyWrites++;
    if (!workflows || !['/internal/v1/model-routes', '/internal/v1/model-routes/route-existing'].includes(path) || !['POST', 'PUT'].includes(method)) throw new Error('Mutation outside the explicit local workflow fixture');
    if (window.failNextFormWrite) { window.failNextFormWrite = false; return new Response(JSON.stringify({ error: { message: '模拟保存失败，草稿仍在' } }), { status: 400 }); }
    const data = JSON.parse(String(init?.body ?? '{}'));
    if (method === 'POST') routeRows = [...routeRows, { ...existingRoute, ...data, id: 'route-created' }];
    else routeRows = routeRows.map(route => route.id === 'route-existing' ? { ...route, ...data } : route);
    return new Response(JSON.stringify({ ...existingRoute, ...data }));
  }
  window.formJourneyReads.push(path);
  const providers = [{ id: 'openai-codex', display_name: 'Codex 订阅', source: 'builtin', protocols: ['openai'],
    credential_schema: { type: 'object', properties: { type: { const: 'oauth' } } }, config_schema: { type: 'object', properties: { base_url: { type: 'string', const: 'https://chatgpt.com/backend-api/codex', readOnly: true } } }, oauth_adapter: { flow_kind: 'openai_device' } },
  { id: 'http-json', display_name: '自部署模型', source: 'builtin', protocols: ['openai'], credential_schema: { type: 'object', properties: { api_key: { type: 'string', title: 'API key', writeOnly: true } } },
    config_schema: { type: 'object', required: ['base_url'], properties: { base_url: { type: 'string', title: 'Base URL' }, timeout_seconds: { type: 'integer', title: 'Timeout seconds', minimum: 1, default: 30 } } } }];
  let value: unknown = [];
  if (path === '/internal/v1/provider-types') value = providers;
  else if (path === '/internal/v1/upstreams') value = workflows ? [account] : [];
  else if (path === '/internal/v1/model-routes') value = workflows ? routeRows : [];
  else if (path.endsWith('/models')) value = { status: 'ready', models: [{ id: 'fixture-model', protocol: 'openai' }] };
  else if (path === '/internal/v1/upstream-models') value = { data: workflows ? [{ id: 'fixture-model', protocol: 'openai', supported_account_count: 1, eligible_account_count: 1, complete_coverage: true }] : [], eligible_account_count: workflows ? 1 : 0, unknown_account_count: 0, stale_account_count: 0 };
  else if (path.includes('monitoring')) value = { contract_version: 'v1', top_upstream_models: [] };
  else if (path.includes('availability')) value = { contract_version: 'upstream_account_availability_v1', accounts: [] };
  return new Response(JSON.stringify(value), { status: 200, headers: { 'Content-Type': 'application/json' } });
};
const routes = new URLSearchParams(location.search).get('view') === 'routes';
function Preview() {
  const [view, setView] = useState<AppRouteKey>(routes ? 'routes' : 'providers');
  const content = view === 'routes' ? <RoutesPage token="mock-only" tenant="fixture" /> : <ProvidersPage token="mock-only" tenant="fixture" />;
  return workflows ? <AppShell surface="operator" route={view} onNavigate={setView}>{content}</AppShell> : <main style={{ padding: 20, maxWidth: 1100, margin: 'auto' }}>{content}</main>;
}
createRoot(document.getElementById('root')!).render(<I18nProvider><MtcFluentProvider><Preview /></MtcFluentProvider></I18nProvider>);
