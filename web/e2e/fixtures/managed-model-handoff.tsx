import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system';
import { RoutesPage } from '../../src/operator/pages/ManagementPages';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/operator.css';

declare global { interface Window { managedHandoffPayloads: Array<Record<string, unknown>> } }
window.managedHandoffPayloads = [];

const accounts = [
  { id: 'browse-account', tenant_external_id: 'fixture-a', name: 'Browse account', driver: 'fixture-provider', status: 'active', connection_method: 'api_key', config: {}, credential_generation: 1, created_at: 1, updated_at: 1 },
  { id: 'foreign-account', tenant_external_id: 'fixture-b', name: 'Foreign account', driver: 'fixture-provider', status: 'active', connection_method: 'api_key', config: {}, credential_generation: 1, created_at: 1, updated_at: 1 },
];

function json(value: unknown, status = 200) {
  return new Response(JSON.stringify(value), { status, headers: { 'Content-Type': 'application/json' } });
}

window.fetch = async (input, init) => {
  const url = new URL(typeof input === 'string' ? input : input instanceof URL ? input.href : input.url, location.origin);
  const method = init?.method ?? (input instanceof Request ? input.method : 'GET');
  if (method === 'POST' && url.pathname === '/internal/v1/model-routes') {
    const payload = JSON.parse(String(init?.body));
    window.managedHandoffPayloads.push(payload);
    return json({ ...payload, id: 'created-route', enabled: true, created_at: 1, updated_at: 1, grant_revision: 0 }, 201);
  }
  if (method !== 'GET') return json({ error: { message: 'unexpected mutation' } }, 404);
  if (url.pathname === '/internal/v1/provider-types') return json([{ id: 'fixture-provider', display_name: 'Fixture provider', protocols: ['openai', 'anthropic'] }]);
  if (url.pathname === '/internal/v1/upstreams') return json(accounts);
  if (url.pathname === '/internal/v1/model-routes' || url.pathname === '/internal/v1/provider-groups' || url.pathname === '/internal/v1/route-groups' || url.pathname === '/internal/v1/keys') return json([]);
  if (url.pathname === '/internal/v1/upstream-models') return json({
    data: [{ id: 'catalog-model-fresh', protocol: 'anthropic', supported_account_count: 1, eligible_account_count: 1, complete_coverage: true, context_window: null, reservation_token_bound: null }],
    eligible_account_count: 1, unknown_account_count: 0, unsupported_account_count: 0, stale_account_count: 0,
  });
  if (url.pathname === '/internal/v1/upstreams/browse-account/models') return json({ status: 'ready', models: [{ id: 'catalog-model-fresh', protocol: 'anthropic' }] });
  return json({ error: { message: `unexpected read: ${url.pathname}` } }, 404);
};

createRoot(document.getElementById('root')!).render(<I18nProvider><MtcFluentProvider><RoutesPage token="fixture-token" tenant="fixture-a" /></MtcFluentProvider></I18nProvider>);
