import { useState } from 'react';
import { createRoot } from 'react-dom/client';

import { I18nProvider } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system';
import { CredentialsPage, ServiceCredentialsPage } from '../../src/operator/pages/ManagementPages';
import keyCreateSchema from '../../../schemas/key-create.schema.json';
import keyPolicySchema from '../../../schemas/key-policy.schema.json';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/operator.css';

type Scenario = 'all-tenants' | 'route-failure' | 'scope-race' | 'scope-lock' | 'client-recovery' | 'service-copy' | 'service-plaintext' | 'service-scope-aba' | 'client-form';

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
const initialTenant = scenario === 'all-tenants' ? '' : 'tenant-a';

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

if (scenario === 'client-recovery') {
  Object.defineProperty(navigator, 'clipboard', { configurable: true, value: {
    writeText: async (value: string) => {
      if (parameters.has('clipboard-failure')) throw new Error('fixture clipboard denied');
      document.documentElement.dataset.copiedFixtureCredential = String(value === 'mts_client_recovered');
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

function credential(alias: string, tenant: string, keyId: string) {
  return {
    key_id: keyId,
    tenant_external_id: tenant,
    principal_external_id: `${tenant}-principal`,
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
    ...(scenario === 'client-form' && typeof init?.body === 'string' ? { body: init.body } : {}),
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
  if (url.pathname === '/internal/v1/service-tokens') return json([{
    service_id: 'service-existing',
    name: 'Existing service credential',
    credential_generation: 1,
    credential_copy_available: scenario === 'service-copy' && !parameters.has('unavailable'),
    fingerprint: 'fixture-fingerprint',
    scopes: ['keys:read'],
    tenant_external_id: initialTenant || 'tenant-a',
    status: 'active',
    created_at: 1_700_000_000_000,
  }]);
  if (url.pathname === '/internal/v1/keys/key-recovery/credential-recovery/copy' && method === 'POST') {
    return json({ key_id: 'key-recovery', credential_generation: 1, key: 'mts_client_recovered' });
  }
  if (scenario === 'client-form') {
    if (url.pathname === '/internal/v1/route-groups') return json([{ id: '00000000-0000-4000-8000-000000000002', name: 'Research group', member_ids: [], member_count: 0, created_at: 1, updated_at: 1 }]);
    if (url.pathname === '/internal/v1/model-routes') return json([{ id: '00000000-0000-4000-8000-000000000001', public_model: 'Research model', enabled: true, tenant_external_id: 'tenant-a' }]);
    if (url.pathname === '/internal/v1/keys' && method === 'POST') return json({ key_id: 'key-created', key: 'mts_fixture_created' });
    if (url.pathname === '/internal/v1/keys/key-form/policy' && method === 'PUT') return json({});
    if (url.pathname === '/internal/v1/keys') return json([credential(localStorage.getItem('mtc-locale')?.startsWith('zh') ? '研发工作区' : 'Research workspace', 'tenant-a', 'key-form')]);
  }
  if (url.pathname.endsWith('credential-groups') || url.pathname.endsWith('route-groups')) return json([]);
  if (url.pathname === '/internal/v1/model-routes') {
    if (scenario === 'route-failure') return json({ error: { message: 'route catalog unavailable' } }, 400);
    return json([]);
  }
  if (url.pathname === '/internal/v1/keys') {
    const tenant = url.searchParams.get('tenant_external_id');
    if (scenario === 'client-recovery') return json([{
      ...credential('Recoverable client', 'tenant-a', 'key-recovery'),
      policy: { ...credential('unused', 'tenant-a', 'unused').policy, enforcement_mode: 'metered_unlimited' },
      credential_recovery_available: true,
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
  if (scenario === 'client-form') return <main style={{ maxWidth: 760, margin: '0 auto', padding: 12 }}><CredentialsPage token="mts_fixture" tenant={tenant} /></main>;
  if (scenario === 'service-plaintext' || scenario === 'service-scope-aba' || scenario === 'service-copy') {
    return <>
      {scenario === 'service-scope-aba' && <button type="button" onClick={() => setTenant((current) => current === 'tenant-a' ? 'tenant-b' : 'tenant-a')}>Switch tenant</button>}
      <span>Tenant {tenant}</span>
      <ServiceCredentialsPage token="mts_fixture" tenant={tenant} />
    </>;
  }
  return <>
    {(scenario === 'scope-race' || scenario === 'scope-lock') && <button type="button" onClick={() => setTenant('tenant-b')}>Switch tenant</button>}
    <CredentialsPage token="mts_fixture" tenant={tenant} />
  </>;
}

createRoot(document.getElementById('root')!).render(<I18nProvider><MtcFluentProvider><Fixture /></MtcFluentProvider></I18nProvider>);
