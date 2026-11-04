import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system';
import { RoutesPage } from '../../src/operator/pages/ManagementPages';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/operator.css';

declare global { interface Window { routeScopePayloads: Array<Record<string, unknown>>; routeScopeDeletes: Array<{ path: string; query: string }> } }
window.routeScopePayloads = [];
window.routeScopeDeletes = [];
const parameters = new URLSearchParams(location.search);
const driver = parameters.get('driver') || 'technical-driver';
const accounts = ['personal', 'team'].map(id => ({ id, tenant_external_id: 'fixture', name: id === 'personal' ? '个人账号' : '团队账号', driver, status: 'active', connection_method: 'oauth', config: {}, credential_generation: 1, created_at: 1, updated_at: 1 }));
const exactId = '00000000-0000-4000-8000-000000000042';
const credential = { key_id: exactId, tenant_external_id: 'fixture', alias: '精确查找凭据', status: 'active', currency: 'USD', created_at: 1 };
const routes: Array<Record<string, unknown>> = parameters.has('existing') ? [{
  id: 'existing-route', tenant_external_id: parameters.has('foreign') ? 'other-tenant' : 'fixture', public_model: 'saved-route', upstream_model: 'kimi-k2', protocol: 'openai', priority: 0,
  upstream_account_ids: ['personal'], included_provider_group_ids: [], excluded_provider_group_ids: [], route_group_ids: ['group-personal'], granted_credential_ids: [exactId],
  custom_model_confirmed: parameters.get('confirmed') === 'true', enabled: !parameters.has('disabled'), grant_revision: 7, created_at: 1, updated_at: 42,
}] : [];
function json(value: unknown, status = 200) { return new Response(JSON.stringify(value), { status, headers: { 'Content-Type': 'application/json' } }); }
window.fetch = async (input, init) => {
  const url = new URL(typeof input === 'string' ? input : input instanceof URL ? input.href : input.url, location.origin);
  const method = init?.method ?? (input instanceof Request ? input.method : 'GET');
  if (method !== 'GET') {
    if (method === 'DELETE' && url.pathname === '/internal/v1/model-routes/existing-route') {
      window.routeScopeDeletes.push({ path: url.pathname, query: url.search });
      routes.splice(0, routes.length);
      return new Response(null, { status: 204 });
    }
    if (!(method === 'POST' && url.pathname === '/internal/v1/model-routes') && !(method === 'PUT' && url.pathname === '/internal/v1/model-routes/existing-route')) throw new Error('Unexpected mock mutation');
    const payload = JSON.parse(String(init?.body)); window.routeScopePayloads.push(payload);
    if (parameters.has('reject')) return json({ error: { code: 'bad_request', message: 'the route has no eligible upstream for this model; sync the model catalog or explicitly confirm a custom model' } }, 400);
    const route = { ...payload, id: 'created-route', enabled: true, grant_revision: 1, created_at: 1, updated_at: 1 };
    if (method === 'PUT') { routes[0] = { ...routes[0], ...payload, updated_at: 43 }; return json(routes[0]); }
    routes.push(route); return json(route, 201);
  }
  if (url.pathname === '/internal/v1/provider-types') return json([{ id: driver, display_name: driver === 'openai-codex' ? 'Codex' : 'Kimi Code', protocols: ['openai'] }]);
  if (url.pathname === '/internal/v1/upstreams') return json(accounts);
  if (url.pathname === '/internal/v1/model-routes') return json(routes);
  if (url.pathname === '/internal/v1/provider-groups') return json(parameters.has('groups') ? [
    { id: 'custom-providers', tenant_external_id: 'fixture', name: '自定义供应商组', member_ids: ['personal', 'team'], member_count: 2, created_at: 1, updated_at: 1 },
    { id: 'excluded-providers', tenant_external_id: 'fixture', name: '排除团队', member_ids: ['team'], member_count: 1, created_at: 1, updated_at: 1 },
  ] : []);
  if (url.pathname === '/internal/v1/route-groups') return json([{ id: 'group-personal', tenant_external_id: 'fixture', name: '个人模型组', member_ids: [], member_count: 0, created_at: 1, updated_at: 1 }]);
  if (url.pathname === '/internal/v1/keys') return json(url.searchParams.get('key_id') === exactId ? [credential] : []);
  if (url.pathname === '/internal/v1/upstream-models') {
    const catalog = parameters.get('catalog');
    if (catalog === 'failed') return json({ error: { message: 'Catalog temporarily unavailable' } }, 503);
    return json({ data: catalog === 'unknown' ? [] : [{ id: 'kimi-k2', protocol: 'openai', supported_account_count: 1, eligible_account_count: catalog === 'partial' ? 2 : 1, complete_coverage: catalog !== 'partial' }], eligible_account_count: catalog === 'partial' ? 2 : 1, unknown_account_count: catalog === 'unknown' || catalog === 'partial' ? 1 : 0, unsupported_account_count: 0, stale_account_count: catalog === 'partial' ? 1 : 0 });
  }
  if (url.pathname.startsWith('/internal/v1/upstreams/') && url.pathname.endsWith('/models')) return json({ status: 'ready', models: [{ id: 'kimi-k2', protocol: 'openai' }] });
  throw new Error(`Unexpected mock read: ${url.pathname}`);
};
createRoot(document.getElementById('root')!).render(<I18nProvider><MtcFluentProvider><main style={{ padding: 16, maxWidth: 1100, margin: 'auto' }}><RoutesPage token="mock" tenant="fixture" writeTenant={parameters.has('read-only') ? '' : 'fixture'} /></main></MtcFluentProvider></I18nProvider>);
