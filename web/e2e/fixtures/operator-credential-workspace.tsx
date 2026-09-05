import { useState } from 'react';
import { createRoot } from 'react-dom/client';

import { I18nProvider } from '../../src/i18n';
import { CredentialsPage } from '../../src/operator/pages/ManagementPages';

type Scenario = 'all-tenants' | 'route-failure' | 'scope-race' | 'scope-lock';

interface FixtureState {
  calls: string[];
}

declare global {
  interface Window { credentialFixture: FixtureState }
}

const parameters = new URLSearchParams(location.search);
const scenario = (parameters.get('scenario') ?? 'all-tenants') as Scenario;
const initialTenant = scenario === 'all-tenants' ? '' : 'tenant-a';

window.credentialFixture = { calls: [] };

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

globalThis.fetch = async (input: RequestInfo | URL) => {
  const url = new URL(typeof input === 'string' ? input : input.toString(), location.origin);
  const call = `${url.pathname}${url.search}`;
  window.credentialFixture.calls.push(call);
  if (url.pathname === '/internal/v1/schemas') {
    return json({ key_create: { type: 'object', properties: {} }, key_policy: { type: 'object', properties: {} } });
  }
  if (url.pathname.endsWith('credential-groups') || url.pathname.endsWith('route-groups')) return json([]);
  if (url.pathname === '/internal/v1/model-routes') {
    if (scenario === 'route-failure') return json({ error: { message: 'route catalog unavailable' } }, 400);
    return json([]);
  }
  if (url.pathname === '/internal/v1/keys') {
    const tenant = url.searchParams.get('tenant_external_id');
    if (scenario === 'all-tenants') return json([credential('All tenant client', 'tenant-visible', 'key-all')]);
    if ((scenario === 'scope-race' || scenario === 'scope-lock') && tenant === 'tenant-a') {
      // Deliberately ignore the aborted signal.  The component must reject this
      // stale result instead of releasing the active tenant-b request.
      return new Promise((resolve) => window.setTimeout(() => resolve(json([
        credential('Scope A client', 'tenant-a', 'key-a'),
      ])), scenario === 'scope-lock' ? 500 : 240));
    }
    if (scenario === 'scope-race' && tenant === 'tenant-b') {
      return new Promise((resolve) => window.setTimeout(() => resolve(json([
        credential('Scope B client', 'tenant-b', 'key-b'),
      ])), 40));
    }
    if (scenario === 'scope-lock' && tenant === 'tenant-b') {
      if (url.searchParams.has('before_id')) {
        return new Promise((resolve) => window.setTimeout(() => resolve(json([
          credential('Scope B older client', 'tenant-b', 'scope-b-001'),
        ])), 700));
      }
      return new Promise((resolve) => window.setTimeout(() => resolve(json(credentialPage('Scope B', 101))), 20));
    }
    return json([credential('Route failure client', 'tenant-a', 'key-route-failure')]);
  }
  if (url.pathname === '/internal/v1/keys/key-all/limits') return json(limitSnapshot('key-all'));
  return json({ error: { message: `unexpected fixture request: ${call}` } }, 404);
};

function Fixture() {
  const [tenant, setTenant] = useState(initialTenant);
  return <>
    {(scenario === 'scope-race' || scenario === 'scope-lock') && <button type="button" onClick={() => setTenant('tenant-b')}>Switch tenant</button>}
    <CredentialsPage token="mts_fixture" tenant={tenant} />
  </>;
}

createRoot(document.getElementById('root')!).render(<I18nProvider><Fixture /></I18nProvider>);
