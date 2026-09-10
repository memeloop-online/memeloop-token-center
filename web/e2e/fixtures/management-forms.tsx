import { useState } from 'react';
import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { CredentialsPage, RoutesPage, ServiceCredentialsPage } from '../../src/operator/pages/ManagementPages';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/operator.css';

window.formFixture = { writes: [], finish: () => {} };
const json = (value: unknown) => new Response(JSON.stringify(value), { headers: { 'Content-Type': 'application/json' } });
const accountId = '11111111-1111-4111-8111-111111111111';
const keySchema = {
  type: 'object', required: ['alias', 'principal_external_id'],
  properties: {
    alias: { type: 'string', title: 'Credential alias', minLength: 1 },
    principal_external_id: { type: 'string', title: 'Principal', minLength: 1 },
    tenant_external_id: { type: 'string', default: 'default' },
    route_ids: { type: 'array', items: { type: 'string' }, default: [] },
    route_group_ids: { type: 'array', items: { type: 'string' }, default: [] },
    currency: { enum: ['USD', 'CNY'], default: 'USD' },
    initial_balance: { type: 'string', title: 'Initial credit', default: '0' },
    policy: { type: 'object', properties: {
      enforcement_mode: { type: 'string', enum: ['prepaid', 'metered_unlimited'], default: 'prepaid' },
      requests_per_minute: { type: 'integer', default: 60, minimum: 1 },
      weekly_budget: { type: ['string', 'null'] },
    } },
  },
};
globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
  const url = new URL(String(input), location.origin);
  if (init?.method && init.method !== 'GET') {
    window.formFixture.writes.push({ path: url.pathname, body: JSON.parse(String(init.body)) });
    return new Promise(resolve => {
      window.formFixture.finish = () => resolve(json({ key: 'fixture-original-key', key_id: 'fixture-key', token: 'fixture-service-token' }));
    });
  }
  if (url.pathname.endsWith('/schemas')) return json({
    key_create: keySchema, key_policy: keySchema.properties.policy,
    service_token: { type: 'object', required: ['name', 'scopes'], properties: {
      name: { type: 'string', minLength: 1 }, tenant_external_id: { type: ['string', 'null'] },
      scopes: { type: 'array', minItems: 1, uniqueItems: true, items: { enum: ['keys:read', 'keys:write', 'routes:read'] } },
    } },
  });
  if (url.pathname.endsWith('/provider-types')) return json([{ id: 'fixture-provider', display_name: 'Fixture Provider', protocols: ['openai'], modalities: ['text'], config_schema: {}, credential_schema: {} }]);
  if (url.pathname.endsWith('/upstreams')) return json([{ id: accountId, tenant_external_id: 'alpha', name: 'Production account', driver: 'fixture-provider', status: 'active' }]);
  if (url.pathname.endsWith('/upstream-models')) return json({
    eligible_account_count: 1, unknown_account_count: 0, stale_account_count: 0,
    data: [{ id: 'fixture-model', protocol: 'openai', supported_account_count: 1, eligible_account_count: 1, complete_coverage: true }],
  });
  if (url.pathname.endsWith('/models')) return json({ status: 'ready', models: [{ id: 'fixture-model', protocol: 'openai' }] });
  return json([]);
};
function Fixture() {
  const [tenant, setTenant] = useState('alpha');
  const view = new URLSearchParams(location.search).get('view');
  const Component = view === 'routes' ? RoutesPage : view === 'services' ? ServiceCredentialsPage : CredentialsPage;
  return <main style={{ padding: 12, minWidth: 0 }}>
    <button onClick={() => setTenant('beta')}>Switch tenant</button>
    <Component token="fixture-control" tenant={tenant} writeTenant={tenant} />
  </main>;
}
createRoot(document.getElementById('root')!).render(<I18nProvider><Fixture /></I18nProvider>);
