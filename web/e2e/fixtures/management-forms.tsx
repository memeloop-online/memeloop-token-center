import { useState } from 'react';
import { createRoot } from 'react-dom/client';
import { flushSync } from 'react-dom';
import { I18nProvider } from '../../src/i18n';
import { CredentialsPage, RoutesPage, ServiceCredentialsPage } from '../../src/operator/pages/ManagementPages';
import { UpstreamModelCombobox } from '../../src/operator/UpstreamModelCombobox';
import type { ProviderType, UpstreamAccount } from '../../src/types';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/operator.css';

window.formFixture = {
  writes: [], reads: [], holdCatalog: false, finish: () => {},
  catalogResolvers: [],
  catalogQueries: [],
  syncRequests: 0, syncActive: 0, syncPeak: 0, cancelledSyncRequests: 0, syncMode: 'ready',
  changeAndSubmit: (change) => {
    flushSync(change);
    document.querySelector<HTMLFormElement>('.create-resource > form')?.requestSubmit();
  },
};
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
  if (url.pathname === '/internal/v1/upstream-models/query') {
    if (init?.method !== 'POST' || url.search) throw new Error('Catalog must use a JSON-body POST with no account IDs in the URL');
    const body = JSON.parse(String(init.body)) as Window['formFixture']['catalogQueries'][number];
    if (!Array.isArray(body.account_ids) || body.account_ids.length > 500
      || !Array.isArray(body.include_provider_group_ids) || body.include_provider_group_ids.length > 100
      || !Array.isArray(body.exclude_provider_group_ids) || body.exclude_provider_group_ids.length > 100) throw new Error('Invalid catalog selection arrays');
    window.formFixture.catalogQueries.push(body);
    const eligibleCount = body.account_ids.length || 500;
    const aggregate = {
      eligible_account_count: eligibleCount, unknown_account_count: 0, stale_account_count: 0,
      data: !body.q || body.q === 'fixture-model'
        ? [{ id: 'fixture-model', protocol: 'openai', supported_account_count: eligibleCount, eligible_account_count: eligibleCount, complete_coverage: true }] : [],
    };
    if (window.formFixture.holdCatalog) return new Promise((resolve, reject) => {
      window.formFixture.catalogResolvers.push(() => resolve(json(aggregate)));
      init.signal?.addEventListener('abort', () => reject(new DOMException('Aborted', 'AbortError')), { once: true });
    });
    return json(aggregate);
  }
  if (url.pathname === '/internal/v1/upstream-models') throw new Error('Legacy GET catalog cannot represent 101–500 explicit candidates');
  if (url.pathname.endsWith('/models/sync') || (url.pathname.endsWith('/models') && !url.searchParams.has('limit'))) {
    window.formFixture.syncRequests += 1;
    window.formFixture.syncActive += 1;
    window.formFixture.syncPeak = Math.max(window.formFixture.syncPeak, window.formFixture.syncActive);
    if (window.formFixture.syncMode === 'hold') return new Promise((_resolve, reject) => {
      init?.signal?.addEventListener('abort', () => {
        window.formFixture.syncActive -= 1;
        window.formFixture.cancelledSyncRequests += 1;
        reject(new DOMException('Aborted', 'AbortError'));
      }, { once: true });
    });
    window.formFixture.syncActive -= 1;
    return json({ status: window.formFixture.syncMode });
  }
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
  window.formFixture.reads.push(url.pathname + url.search);
  if (url.pathname.endsWith('/upstreams')) return json([
    { id: accountId, tenant_external_id: 'alpha', name: 'Production account', driver: 'fixture-provider', status: 'active' },
    { id: '22222222-2222-4222-8222-222222222222', tenant_external_id: 'alpha', name: 'Backup account', driver: 'fixture-provider', status: 'active' },
  ]);
  if (url.pathname.endsWith('/models')) return json({ status: 'ready', models: [{ id: 'fixture-model', protocol: 'openai' }] });
  return json([]);
};
const largeAccounts = Array.from({ length: 500 }, (_, index) => ({
  id: `00000000-0000-4000-8000-${String(index).padStart(12, '0')}`, name: `Account ${index}`, driver: 'fixture-provider', status: 'active',
})) as UpstreamAccount[];
function LargeCatalog() {
  const explicitCount = Number(new URLSearchParams(location.search).get('explicit'));
  const [members, setMembers] = useState(largeAccounts.slice(0, explicitCount || 500).map(account => account.id));
  return <>
    <button onClick={() => setMembers(members.slice(0, -1))}>Change group membership</button>
    <UpstreamModelCombobox token="fixture-control" tenant="alpha" protocol="openai"
      accountIds={explicitCount ? members : []} includedProviderGroupIds={explicitCount ? [] : ['11111111-1111-4111-8111-111111111111']} excludedProviderGroupIds={[]}
      syncAccountIds={members} value="fixture-model" onChange={() => {}} customModelConfirmed={false}
      onValidityChange={() => {}} upstreams={largeAccounts}
      providers={[{ id: 'fixture-provider', display_name: 'Fixture Provider' } as ProviderType]} />
  </>;
}
function Fixture() {
  const [tenant, setTenant] = useState('alpha');
  const view = new URLSearchParams(location.search).get('view');
  if (view === 'large-catalog') return <LargeCatalog />;
  const Component = view === 'routes' ? RoutesPage : view === 'services' ? ServiceCredentialsPage : CredentialsPage;
  return <main style={{ padding: 12, minWidth: 0 }}>
    <button onClick={() => setTenant('beta')}>Switch tenant</button>
    <Component token="fixture-control" tenant={tenant} writeTenant={tenant} />
  </main>;
}
createRoot(document.getElementById('root')!).render(<I18nProvider><Fixture /></I18nProvider>);
