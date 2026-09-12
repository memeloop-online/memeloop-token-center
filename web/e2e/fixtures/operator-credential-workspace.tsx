import { useState } from 'react';
import { createRoot } from 'react-dom/client';

import { I18nProvider } from '../../src/i18n';
import { CredentialsPage, ServiceCredentialsPage } from '../../src/operator/pages/ManagementPages';

type Scenario = 'all-tenants' | 'route-failure' | 'scope-race' | 'scope-lock' | 'client-recovery' | 'service-plaintext' | 'service-scope-aba';

interface RecordedRequest {
  method: string;
  path: string;
  cache?: RequestCache;
  credentials?: RequestCredentials;
  referrerPolicy?: ReferrerPolicy;
  hasSignal: boolean;
}

interface FixtureState {
  calls: string[];
  requests: RecordedRequest[];
  releaseIssue: (token: string) => void;
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
const pendingCredentialScopeA: Array<(response: Response) => void> = [];
const pendingCredentialCursor: Array<(response: Response) => void> = [];
window.credentialFixture = {
  calls: [],
  requests: [],
  createdObjectUrls: [],
  revokedObjectUrls: [],
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
  });
  if (url.pathname === '/internal/v1/schemas') {
    return json({
      key_create: { type: 'object', properties: {} },
      key_policy: { type: 'object', properties: {} },
      service_token: { type: 'object', properties: {} },
    });
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
    fingerprint: 'fixture-fingerprint',
    scopes: ['keys:read'],
    tenant_external_id: initialTenant || 'tenant-a',
    status: 'active',
    created_at: 1_700_000_000_000,
  }]);
  if (url.pathname === '/internal/v1/keys/key-recovery/credential-recovery/copy' && method === 'POST') {
    return json({ key_id: 'key-recovery', credential_generation: 1, key: 'mts_client_recovered' });
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
      credential_recovery_available: true,
    }]);
    if (scenario === 'all-tenants') return json([credential('All tenant client', 'tenant-visible', 'key-all')]);
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
  if (scenario === 'service-plaintext' || scenario === 'service-scope-aba') {
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

createRoot(document.getElementById('root')!).render(<I18nProvider><Fixture /></I18nProvider>);
