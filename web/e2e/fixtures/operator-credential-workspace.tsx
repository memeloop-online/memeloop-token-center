import { useState } from 'react';
import { createRoot } from 'react-dom/client';

import { I18nProvider, useI18n } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system';
import { CredentialsPage, ServiceCredentialsPage } from '../../src/operator/pages/ManagementPages';
import keyCreateSchema from '../../../schemas/key-create.schema.json';
import keyPolicySchema from '../../../schemas/key-policy.schema.json';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/operator.css';

type Scenario = 'all-tenants' | 'route-failure' | 'scope-race' | 'scope-lock' | 'client-copy' | 'service-copy' | 'service-plaintext' | 'service-scope-aba' | 'service-edit' | 'client-form' | 'client-filter' | 'client-lifecycle';

interface RecordedRequest {
  method: string;
  path: string;
  cache?: RequestCache;
  credentials?: RequestCredentials;
  referrerPolicy?: ReferrerPolicy;
  hasSignal: boolean;
  body?: string;
}

interface FixtureState {
  calls: string[];
  requests: RecordedRequest[];
  releaseIssue: (token: string) => void;
  releaseRoutingResponse: (status: number) => void;
  releaseCredentialScopeA: () => void;
  releaseCredentialCursor: () => void;
  createdObjectUrls: string[];
  revokedObjectUrls: string[];
}

declare global {
  interface Window { credentialFixture: FixtureState }
}

const parameters = new URLSearchParams(location.search);
const scenario = (parameters.get('scenario') ?? 'all-tenants') as Scenario;
const initialTenant = scenario === 'all-tenants' ? '' : scenario === 'client-lifecycle' ? 'default' : 'tenant-a';
const deletedClientIds = new Set<string>();

let serviceEditState = {
  service_id: 'service-existing',
  name: 'Existing service credential',
  credential_generation: 1,
  credential_copy_available: false,
  fingerprint: 'fixture-fingerprint',
  scopes: ['keys:read'],
  tenant_external_id: 'tenant-a' as string | null,
  status: 'active',
  created_at: 1_700_000_000_000,
};

const pendingIssues: Array<(response: Response) => void> = [];
const pendingRouting: Array<(response: Response) => void> = [];
const routingResponse = { key_id: 'key-form', route_ids: [], route_group_ids: [], effective_route_ids: [], grant_revision: 1, updated_at: 1 };
const pendingCredentialScopeA: Array<(response: Response) => void> = [];
const pendingCredentialCursor: Array<(response: Response) => void> = [];
window.credentialFixture = {
  calls: [],
  requests: [],
  createdObjectUrls: [],
  revokedObjectUrls: [],
  releaseRoutingResponse(status) {
    const resolve = pendingRouting.shift();
    if (!resolve) throw new Error('no pending routing fixture response');
    resolve(json(status === 200 ? routingResponse : { error: { message: 'late routing conflict must stay hidden' } }, status));
  },
  releaseIssue(token) {
    const resolve = pendingIssues.shift();
    if (!resolve) throw new Error('no pending service credential issuance');
    resolve(json({ token }));
  },
  releaseCredentialScopeA() {
    const resolve = pendingCredentialScopeA.shift();
    if (!resolve) throw new Error('no pending tenant-a credential page');
    resolve(json([credential('Scope A client', 'tenant-a', 'key-a')]));
  },
  releaseCredentialCursor() {
    const resolve = pendingCredentialCursor.shift();
    if (!resolve) throw new Error('no pending tenant-b credential cursor');
    resolve(json([credential('Scope B older client', 'tenant-b', 'scope-b-001')]));
  },
};

if (scenario === 'client-copy') {
  Object.defineProperty(navigator, 'clipboard', { configurable: true, value: {
    writeText: async (value: string) => {
      if (parameters.has('clipboard-failure')) throw new Error('fixture clipboard denied');
      document.documentElement.dataset.copiedFixtureCredential = String(value === 'mtc_client_copied');
    },
  } });
}
if (scenario === 'service-copy') {
  Object.defineProperty(navigator, 'clipboard', { configurable: true, value: {
    writeText: async (value: string) => {
      if (parameters.has('clipboard-failure')) throw new Error('fixture clipboard denied');
      document.documentElement.dataset.copiedServiceFixture = String(value === 'mts_service_original');
    },
  } });
}

if (scenario === 'service-plaintext') {
  Object.defineProperty(navigator, 'clipboard', { configurable: true, value: undefined });
  document.execCommand = () => { throw new Error('fixture clipboard failure'); };
  URL.createObjectURL = () => {
    const url = `blob:credential-fixture-${window.credentialFixture.createdObjectUrls.length + 1}`;
    window.credentialFixture.createdObjectUrls.push(url);
    return url;
  };
  URL.revokeObjectURL = (url) => { window.credentialFixture.revokedObjectUrls.push(url); };
}

function json(value: unknown, status = 200) {
  return new Response(JSON.stringify(value), {
    status,
    headers: { 'Content-Type': 'application/json' },
  });
}

function credential(alias: string, tenant: string, keyId: string, principal?: string) {
  return {
    key_id: keyId,
    tenant_external_id: tenant,
    principal_external_id: principal ?? `${tenant}-principal`,
    alias,
    status: 'active',
    currency: 'USD',
    credential_generation: 1,
    created_at: 1_700_000_000_000,
    available_balance: '10',
    policy: {
      requests_per_minute: 10,
      tokens_per_minute: 100,
      max_concurrency: 1,
      enforcement_mode: 'prepaid',
      daily_budget: null,
      weekly_budget: null,
      lifetime_budget: null,
    },
  };
}

function credentialPage(prefix: string, count: number) {
  return Array.from({ length: count }, (_, index) => credential(
    `${prefix} ${count - index}`,
    'tenant-b',
    `${prefix.toLowerCase().replaceAll(' ', '-')}-${String(count - index).padStart(3, '0')}`,
  ));
}

function limitSnapshot(keyId: string) {
  const rate = { limit: 10, used: 0, remaining: 10, reset_at: 1_700_000_060_000 };
  const budget = { limit: null, settled: '0', reserved: '0', remaining: null, reset_at: null };
  return {
    key_id: keyId,
    captured_at: 1_700_000_000_000,
    currency: 'USD',
    available_balance: '10',
    reserved_balance: '0',
    rpm: rate,
    tpm: rate,
    concurrency: { limit: 1, active: 0, remaining: 1 },
    daily_budget: budget,
    weekly_budget: budget,
    lifetime_budget: budget,
  };
}

globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
  const url = new URL(typeof input === 'string' ? input : input.toString(), location.origin);
  const call = `${url.pathname}${url.search}`;
  const method = init?.method ?? 'GET';
  window.credentialFixture.calls.push(call);
  window.credentialFixture.requests.push({
    method,
    path: call,
    cache: init?.cache,
    credentials: init?.credentials,
    referrerPolicy: init?.referrerPolicy,
    hasSignal: Boolean(init?.signal),
    ...((scenario === 'client-form' || scenario === 'client-lifecycle' || scenario === 'service-edit') && typeof init?.body === 'string' ? { body: init.body } : {}),
  });
  if (url.pathname === '/internal/v1/schemas') {
    return json({
      key_create: scenario === 'client-form' ? keyCreateSchema : { type: 'object', properties: {} },
      key_policy: scenario === 'client-form' ? keyPolicySchema : { type: 'object', properties: {} },
      service_token: { type: 'object', properties: {} },
    });
  }
  if (parameters.has('routing-lifecycle') && url.pathname === '/internal/v1/keys/key-form/routing') {
    return new Promise<Response>(resolve => pendingRouting.push(resolve));
  }
  if (url.pathname === '/internal/v1/service-tokens/service-existing/copy' && method === 'POST' && scenario === 'service-copy') {
    if (parameters.has('forbidden')) return json({ error: { message: 'fixture permission denied' } }, 403);
    return json({ service_id: 'service-existing', credential_generation: 1, token: 'mts_service_original' });
  }
  if (url.pathname === '/internal/v1/service-tokens' && method === 'POST') {
    // Deliberately ignore AbortSignal so the component, rather than the mock,
    // must fence a response from an old tenant/auth epoch.
    return new Promise<Response>((resolve) => pendingIssues.push(resolve));
  }
  if (url.pathname === '/internal/v1/service-tokens/service-existing' && method === 'PATCH' && scenario === 'service-edit') {
    if (parameters.has('forbidden')) return json({ error: { code: 'forbidden', message: 'fixture global service_tokens:write required' } }, 403);
    const body = JSON.parse(String(init?.body ?? '{}'));
    serviceEditState = {
      ...serviceEditState,
      name: typeof body.name === 'string' ? body.name : serviceEditState.name,
      scopes: Array.isArray(body.scopes) ? body.scopes : serviceEditState.scopes,
      tenant_external_id: body.tenant_external_id === undefined ? serviceEditState.tenant_external_id : body.tenant_external_id,
    };
    return json(serviceEditState);
  }
  if (url.pathname === '/internal/v1/service-tokens') return json([{
    service_id: 'service-existing',
    name: scenario === 'service-edit' ? serviceEditState.name : 'Existing service credential',
    credential_generation: 1,
    credential_copy_available: scenario === 'service-copy' && !parameters.has('missing-original'),
    fingerprint: 'fixture-fingerprint',
    scopes: scenario === 'service-edit' ? serviceEditState.scopes : ['keys:read'],
    tenant_external_id: scenario === 'service-edit' ? serviceEditState.tenant_external_id : (initialTenant || 'tenant-a'),
    status: parameters.has('revoked') ? 'revoked' : parameters.has('suspended') ? 'suspended' : 'active',
    created_at: 1_700_000_000_000,
  }]);
  if (url.pathname === '/internal/v1/keys/key-copy/copy' && method === 'POST') {
    return json({ key_id: 'key-copy', credential_generation: 1, key: 'mtc_client_copied' });
  }
  if (url.pathname === '/internal/v1/keys/key-copy/credential' && method === 'PUT') {
    const body = JSON.parse(String(init?.body));
    return body.key === 'mtc_client_known_original' ? json({}) : json({ error: { message: 'fixture original did not match' } }, 400);
  }
  if (scenario === 'client-lifecycle' && url.pathname === '/internal/v1/keys/delete') {
    const body = JSON.parse(String(init?.body));
    if (body.tenant_external_id !== 'default' || body.key_ids.length > 100) return json({}, 400);
    for (const id of body.key_ids) deletedClientIds.add(id);
    return json({ deleted_key_ids: body.key_ids });
  }
  if (scenario === 'client-form') {
    if (url.pathname === '/internal/v1/upstreams' && parameters.has('account-catalog-failure') && document.documentElement.dataset.accountCatalogAvailable !== 'true') return json({ error: { message: 'fixture catalog unavailable' } }, 503);
    if (url.pathname === '/internal/v1/keys/key-form/routing') return json({ ...routingResponse, route_ids: ['00000000-0000-4000-8000-000000000004'], effective_route_ids: ['00000000-0000-4000-8000-000000000004'] });
    if (url.pathname === '/internal/v1/provider-types') return json([{ id: 'kimi', display_name: 'Kimi' }]);
    if (url.pathname === '/internal/v1/upstreams') return json([{ id: 'account-personal', name: 'Personal Kimi', driver: 'kimi', status: 'active' }, { id: 'account-team', name: 'Team Kimi', driver: 'kimi', status: 'active' }]);
    if (url.pathname === '/internal/v1/route-groups') return json([{ id: '00000000-0000-4000-8000-000000000002', name: 'Research group', member_ids: ['00000000-0000-4000-8000-000000000001'], member_count: 1, created_at: 1, updated_at: 1 }]);
    if (url.pathname === '/internal/v1/model-routes' && parameters.has('paged-route-catalog')) {
      // The granted route sorts outside the first keyset page; the workspace
      // must keep paging before it can name the grant.
      const pagedCatalog = [
        ...Array.from({ length: 150 }, (_, index) => ({ id: `00000000-0000-4000-8000-${String(index + 100).padStart(12, '0')}`, public_model: `Paged model ${index + 1}`, upstream_model: 'kimi-paged', protocol: 'openai', enabled: true, tenant_external_id: 'tenant-a', candidate_upstream_account_ids: ['account-personal'], created_at: 10_000 - index })),
        { id: '00000000-0000-4000-8000-000000000004', public_model: 'Spark disabled', upstream_model: 'kimi-spark', protocol: 'openai', enabled: false, tenant_external_id: 'tenant-a', candidate_upstream_account_ids: [], created_at: 1_000 },
      ];
      const beforeCreatedAt = Number(url.searchParams.get('before_created_at') ?? Number.MAX_SAFE_INTEGER);
      const beforeId = url.searchParams.get('before_id') ?? 'ffffffff-ffff-ffff-ffff-ffffffffffff';
      const limit = Number(url.searchParams.get('limit') ?? '100');
      return json(pagedCatalog.filter((route) => route.created_at < beforeCreatedAt || (route.created_at === beforeCreatedAt && route.id < beforeId)).slice(0, limit));
    }
    if (url.pathname === '/internal/v1/model-routes' && parameters.has('paged-route-catalog-exact')) {
      // Exactly two full pages. The loader must issue one more read, accept
      // the empty answer as the end of the catalog, and still resolve the
      // grant sitting at the page boundary.
      const exactCatalog = [
        ...Array.from({ length: 199 }, (_, index) => ({ id: `00000000-0000-4000-8000-${String(index + 100).padStart(12, '0')}`, public_model: `Exact model ${index + 1}`, upstream_model: 'kimi-exact', protocol: 'openai', enabled: true, tenant_external_id: 'tenant-a', candidate_upstream_account_ids: ['account-personal'], created_at: 20_000 - index })),
        { id: '00000000-0000-4000-8000-000000000004', public_model: 'Spark disabled', upstream_model: 'kimi-spark', protocol: 'openai', enabled: false, tenant_external_id: 'tenant-a', candidate_upstream_account_ids: [], created_at: 1_000 },
      ];
      const beforeCreatedAt = Number(url.searchParams.get('before_created_at') ?? Number.MAX_SAFE_INTEGER);
      const beforeId = url.searchParams.get('before_id') ?? 'ffffffff-ffff-ffff-ffff-ffffffffffff';
      const limit = Number(url.searchParams.get('limit') ?? '100');
      return json(exactCatalog.filter((route) => route.created_at < beforeCreatedAt || (route.created_at === beforeCreatedAt && route.id < beforeId)).slice(0, limit));
    }
    if (url.pathname === '/internal/v1/model-routes' && parameters.has('paged-route-catalog-stuck')) {
      // Always answers the same full page regardless of the cursor. The
      // loader must report an incomplete catalog instead of paging forever.
      const stuckPage = Array.from({ length: 100 }, (_, index) => ({ id: `00000000-0000-4000-8000-${String(index + 100).padStart(12, '0')}`, public_model: `Stuck model ${index + 1}`, upstream_model: 'kimi-stuck', protocol: 'openai', enabled: true, tenant_external_id: 'tenant-a', candidate_upstream_account_ids: ['account-personal'], created_at: 30_000 - index }));
      return json(stuckPage);
    }
    if (url.pathname === '/internal/v1/model-routes') {
      const olderRoutes = [
        { id: '00000000-0000-4000-8000-000000000001', public_model: 'Research model', upstream_model: 'kimi-research', protocol: 'openai', enabled: true, tenant_external_id: 'tenant-a', candidate_upstream_account_ids: ['account-personal'] },
        { id: '00000000-0000-4000-8000-000000000003', public_model: 'Research model', upstream_model: 'kimi-research', protocol: 'openai', enabled: true, tenant_external_id: 'tenant-a', candidate_upstream_account_ids: ['account-team'] },
        { id: '00000000-0000-4000-8000-000000000004', public_model: 'Spark disabled', enabled: false, tenant_external_id: 'tenant-a', candidate_upstream_account_ids: [] },
        { id: '00000000-0000-4000-8000-000000000005', public_model: 'Empty route', enabled: true, tenant_external_id: 'tenant-a', candidate_upstream_account_ids: [] },
        ...Array.from({ length: 7 }, (_, index) => ({ id: `00000000-0000-4000-8000-00000000001${index}`, public_model: 'Research model', upstream_model: 'kimi-research', protocol: 'openai', enabled: false, tenant_external_id: 'tenant-a', candidate_upstream_account_ids: ['account-personal'] })),
      ];
      if (!parameters.has('route-pages')) return json(olderRoutes);
      if (!url.searchParams.has('before_id')) return json(Array.from({ length: 100 }, (_, index) => ({
        ...olderRoutes[0], id: `new-route-${String(100 - index).padStart(3, '0')}`, public_model: `Recent model ${index}`, created_at: 2_000 - index,
      })));
      if (parameters.has('route-page-failure') && document.documentElement.dataset.routeCatalogAvailable !== 'true') return json({ error: { message: 'route catalog unavailable' } }, 503);
      return json([...olderRoutes, { ...olderRoutes[0], id: '00000000-0000-4000-8000-000000000006', public_model: 'gpt-6-astra' }]);
    }
    if (url.pathname === '/internal/v1/keys' && method === 'POST') {
      if (parameters.has('duplicate-alias')) return json({ error: { message: 'fixture duplicate alias' } }, 409);
      return json({ key_id: 'key-created', key: 'mtc_fixture_created' });
    }
    if (url.pathname === '/internal/v1/keys/key-form/policy' && method === 'PUT') return json({});
    if (url.pathname === '/internal/v1/keys/key-form/alias' && method === 'PATCH') return json({});
    if (/^\/internal\/v1\/keys\/[^/]+\/routing$/.test(url.pathname) && url.pathname !== '/internal/v1/keys/key-form/routing') {
      return json({ ...routingResponse, key_id: url.pathname.split('/')[4] });
    }
    if (url.pathname === '/internal/v1/keys') return json([
      // Match the API's descending (created_at, key_id) keyset order.
      // These rows share a timestamp, and key-other sorts before key-form.
      ...(parameters.has('multiple-policies') ? [credential('Other workspace', 'tenant-a', 'key-other')] : []),
      // A legacy identity whose external ID legitimately carries surrounding
      // whitespace; picking it must bind the exact ID, never a trimmed rewrite.
      ...(parameters.has('padded-principal') ? [credential('Padded owner client', 'tenant-a', 'key-padded', ' padded-owner ')] : []),
      credential(localStorage.getItem('mtc-locale')?.startsWith('zh') ? '研发工作区' : 'Research workspace', 'tenant-a', 'key-form'),
    ]);
    if (url.pathname === '/internal/v1/keys/key-form/rotate' && method === 'POST') return json({ key: 'mtc_fixture_rotated' });
  }
  if (url.pathname.endsWith('credential-groups') || url.pathname.endsWith('route-groups')) return json([]);
  if (url.pathname === '/internal/v1/model-routes') {
    if (scenario === 'route-failure') return json({ error: { message: 'route catalog unavailable' } }, 400);
    return json([]);
  }
  if (url.pathname === '/internal/v1/keys') {
    if (scenario === 'client-lifecycle') {
      const source = url.searchParams.get('creation_source');
      const rows = [
        { ...credential('Manual workspace', 'default', 'key-manual'), creation_source: 'manual' },
        { ...credential('API workspace', 'default', 'key-api'), creation_source: 'api' },
      ];
      return json(rows.filter(row => !deletedClientIds.has(row.key_id) && (!source || row.creation_source === source || (source === 'manual_or_unknown' && row.creation_source === 'manual'))));
    }
    if (scenario === 'client-filter') {
      const search = url.searchParams.get('search');
      const status = url.searchParams.get('status');
      return json([{
        ...credential(search === '100%_literal' ? 'Matching older client' : 'Recent client', 'tenant-a', 'key-filter'),
        status: status ?? 'revoked',
      }]);
    }
    const tenant = url.searchParams.get('tenant_external_id');
    if (scenario === 'client-copy') return json([{
      ...credential('Copyable client', 'tenant-a', 'key-copy'),
      status: parameters.has('revoked') ? 'revoked' : parameters.has('suspended') ? 'suspended' : 'active',
      policy: { ...credential('unused', 'tenant-a', 'unused').policy, enforcement_mode: 'metered_unlimited' },
      credential_copy_available: !parameters.has('missing-original'),
    }]);
    if (scenario === 'all-tenants') return json([{
      ...credential('All tenant client', 'tenant-visible', 'key-all'),
      available_balance: '9223372036854.775807',
    }]);
    if ((scenario === 'scope-race' || scenario === 'scope-lock') && tenant === 'tenant-a') {
      // Deliberately ignore the aborted signal.  The component must reject this
      // stale result instead of releasing the active tenant-b request.
      return new Promise<Response>((resolve) => pendingCredentialScopeA.push(resolve));
    }
    if (scenario === 'scope-race' && tenant === 'tenant-b') {
      return json([credential('Scope B client', 'tenant-b', 'key-b')]);
    }
    if (scenario === 'scope-lock' && tenant === 'tenant-b') {
      if (url.searchParams.has('before_id')) {
        return new Promise<Response>((resolve) => pendingCredentialCursor.push(resolve));
      }
      return json(credentialPage('Scope B', 101));
    }
    return json([credential('Route failure client', 'tenant-a', 'key-route-failure')]);
  }
  if (url.pathname === '/internal/v1/keys/key-all/limits') return json(limitSnapshot('key-all'));
  return json({ error: { message: `unexpected fixture request: ${call}` } }, 404);
};

function Fixture() {
  const [tenant, setTenant] = useState(initialTenant);
  const { locale, setLocale } = useI18n();
  if (scenario === 'client-form') return <main style={{ maxWidth: 960, margin: '0 auto', padding: 12 }}><button data-change-locale type="button" onClick={() => setLocale(locale === 'en' ? 'zh-CN' : 'en')}>Switch language</button><CredentialsPage token="mts_fixture" tenant={tenant} /></main>;
  if (scenario === 'service-plaintext' || scenario === 'service-scope-aba' || scenario === 'service-copy' || scenario === 'service-edit') {
    return <>
      {scenario === 'service-scope-aba' && <button type="button" onClick={() => setTenant((current) => current === 'tenant-a' ? 'tenant-b' : 'tenant-a')}>Switch tenant</button>}
      <span>Tenant {tenant}</span>
      <ServiceCredentialsPage token="mts_fixture" tenant={tenant} tenants={[{ external_id: 'tenant-a' }, { external_id: 'tenant-b' }]} />
    </>;
  }
  return <>
    {(scenario === 'scope-race' || scenario === 'scope-lock') && <button type="button" onClick={() => setTenant('tenant-b')}>Switch tenant</button>}
    <CredentialsPage token="mts_fixture" tenant={tenant} />
  </>;
}

createRoot(document.getElementById('root')!).render(<I18nProvider><MtcFluentProvider><Fixture /></MtcFluentProvider></I18nProvider>);
