import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system';
import { RoutesPage } from '../../src/operator/pages/ManagementPages';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/operator.css';

declare global { interface Window { routeScopePayloads: Array<Record<string, unknown>> } }
window.routeScopePayloads = [];
const accounts = ['personal', 'team'].map(id => ({ id, tenant_external_id: 'fixture', name: id === 'personal' ? '个人账号' : '团队账号', driver: 'technical-driver', status: 'active', connection_method: 'oauth', config: {}, credential_generation: 1, created_at: 1, updated_at: 1 }));
const exactId = '00000000-0000-4000-8000-000000000042';
const credential = { key_id: exactId, tenant_external_id: 'fixture', alias: '精确查找凭据', status: 'active', currency: 'USD', created_at: 1 };
const routes: Array<Record<string, unknown>> = [];
function json(value: unknown, status = 200) { return new Response(JSON.stringify(value), { status, headers: { 'Content-Type': 'application/json' } }); }
window.fetch = async (input, init) => {
  const url = new URL(typeof input === 'string' ? input : input instanceof URL ? input.href : input.url, location.origin);
  const method = init?.method ?? (input instanceof Request ? input.method : 'GET');
  if (method !== 'GET') {
    if (method !== 'POST' || url.pathname !== '/internal/v1/model-routes') throw new Error('Unexpected mock mutation');
    const payload = JSON.parse(String(init?.body)); window.routeScopePayloads.push(payload);
    const route = { ...payload, id: 'created-route', enabled: true, grant_revision: 1, created_at: 1, updated_at: 1 };
    routes.push(route); return json(route, 201);
  }
  if (url.pathname === '/internal/v1/provider-types') return json([{ id: 'technical-driver', display_name: 'Kimi Code', protocols: ['openai'] }]);
  if (url.pathname === '/internal/v1/upstreams') return json(accounts);
  if (url.pathname === '/internal/v1/model-routes') return json(routes);
  if (url.pathname === '/internal/v1/provider-groups') return json([]);
  if (url.pathname === '/internal/v1/route-groups') return json([{ id: 'group-personal', tenant_external_id: 'fixture', name: '个人模型组', member_ids: [], member_count: 0, created_at: 1, updated_at: 1 }]);
  if (url.pathname === '/internal/v1/keys') return json(url.searchParams.get('key_id') === exactId ? [credential] : []);
  if (url.pathname === '/internal/v1/upstream-models') return json({ data: [{ id: 'kimi-k2', protocol: 'openai', supported_account_count: 1, eligible_account_count: 1, complete_coverage: true }], eligible_account_count: 1, unknown_account_count: 0, unsupported_account_count: 0, stale_account_count: 0 });
  if (url.pathname.startsWith('/internal/v1/upstreams/') && url.pathname.endsWith('/models')) return json({ status: 'ready', models: [{ id: 'kimi-k2', protocol: 'openai' }] });
  throw new Error(`Unexpected mock read: ${url.pathname}`);
};
createRoot(document.getElementById('root')!).render(<I18nProvider><MtcFluentProvider><main style={{ padding: 16, maxWidth: 1100, margin: 'auto' }}><RoutesPage token="mock" tenant="fixture" /></main></MtcFluentProvider></I18nProvider>);
