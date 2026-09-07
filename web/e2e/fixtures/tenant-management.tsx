import { useEffect, useState } from 'react';
import { createRoot } from 'react-dom/client';

import { AppShell } from '../../src/app/AppShell';
import { operatorRouteKeys, type OperatorRouteKey } from '../../src/app/routes';
import { I18nProvider } from '../../src/i18n';
import { Operator } from '../../src/operator/Operator';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/app-shell.css';

type Scenario = 'default' | 'multiple' | 'denied';

interface TenantRecord {
  external_id: string;
  status: 'active' | 'archived';
}

declare global {
  interface Window { tenantFixture: { calls: string[] } }
}

const parameters = new URLSearchParams(location.search);
const scenario = (parameters.get('scenario') ?? 'default') as Scenario;
const fixtureCredential = 'mts_tenant_fixture';
let tenantRecords: TenantRecord[] = scenario === 'multiple'
  ? [{ external_id: 'default', status: 'active' }, { external_id: 'north', status: 'active' }]
  : [{ external_id: 'default', status: 'active' }];

window.tenantFixture = { calls: [] };

function json(value: unknown, status = 200) {
  return new Response(JSON.stringify(value), {
    status,
    headers: { 'Content-Type': 'application/json' },
  });
}

function currentRoute(): OperatorRouteKey {
  const route = new URL(location.href).searchParams.get('view');
  return (operatorRouteKeys as readonly string[]).includes(route ?? '') ? route as OperatorRouteKey : 'overview';
}

function managementTenant(pathname: string) {
  return decodeURIComponent(pathname.slice('/internal/v1/tenant-management/'.length).split('/')[0] ?? '');
}

globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
  const url = new URL(typeof input === 'string' ? input : input.toString(), location.origin);
  const method = init?.method ?? 'GET';
  const call = `${method} ${url.pathname}${url.search}`;
  window.tenantFixture.calls.push(call);
  if (url.pathname === '/internal/v1/tenants') {
    if (scenario === 'denied') return json({ error: { message: 'Tenant access denied' } }, 403);
    return json(tenantRecords.filter((tenant) => tenant.status === 'active').map(({ external_id }) => ({ external_id })));
  }
  if (url.pathname === '/internal/v1/tenant-management') {
    if (method === 'GET') return json(tenantRecords);
    if (method === 'POST') {
      const body = JSON.parse(String(init?.body ?? '{}')) as { external_id?: string };
      const created = { external_id: body.external_id?.trim() || 'unnamed', status: 'active' as const };
      tenantRecords = [...tenantRecords, created];
      return json(created);
    }
  }
  if (url.pathname.startsWith('/internal/v1/tenant-management/')) {
    const tenantId = managementTenant(url.pathname);
    const action = url.pathname.split('/').at(-1);
    const tenant = tenantRecords.find((value) => value.external_id === tenantId);
    if (!tenant) return json({ error: { message: 'Tenant not found' } }, 404);
    if (method === 'PATCH') {
      const body = JSON.parse(String(init?.body ?? '{}')) as { external_id?: string };
      tenant.external_id = body.external_id?.trim() || tenant.external_id;
      return json(tenant);
    }
    if (method === 'POST' && (action === 'archive' || action === 'restore')) {
      tenant.status = action === 'archive' ? 'archived' : 'active';
      return json(tenant);
    }
    if (method === 'DELETE') return json({ error: { message: 'Tenant still owns resources' } }, 409);
  }
  return json([]);
};

function Fixture() {
  const [route, setRoute] = useState<OperatorRouteKey>(currentRoute);

  useEffect(() => {
    const onPopState = () => setRoute(currentRoute());
    window.addEventListener('popstate', onPopState);
    return () => window.removeEventListener('popstate', onPopState);
  }, []);

  function navigate(next: OperatorRouteKey) {
    const url = new URL(location.href);
    url.searchParams.set('view', next);
    window.history.pushState(null, '', `${url.pathname}${url.search}`);
    setRoute(next);
  }

  return <AppShell surface="operator" route={route} onNavigate={navigate}>
    <Operator route={route} onRouteChange={navigate} embedded showNavigation={false} />
  </AppShell>;
}

localStorage.setItem('mtc.operator.service-credential.v1', fixtureCredential);
createRoot(document.getElementById('root')!).render(<I18nProvider><Fixture /></I18nProvider>);
