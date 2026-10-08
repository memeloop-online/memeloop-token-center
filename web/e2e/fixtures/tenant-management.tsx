import { createRoot } from 'react-dom/client';

import { AppShell } from '../../src/app/AppShell';
import { useAppLocation } from '../../src/app/useAppLocation';
import { isPluginRouteKey } from '../../src/app/routes';
import { isOperatorRouteKey } from '../../src/operator/scope/operatorRoutes';
import { I18nProvider } from '../../src/i18n';
import { Operator } from '../../src/operator/Operator';
import { MtcFluentProvider } from '../../src/design-system';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/app-shell.css';

type Scenario = 'default' | 'multiple' | 'denied' | 'empty' | 'slow' | 'management-denied' | 'management-error' | 'background-refresh' | 'services-denied' | 'services-empty';

interface TenantRecord {
  external_id: string;
  status: 'active' | 'archived';
}

declare global {
  interface Window { tenantFixture: { calls: string[]; holdNext?: boolean; release?: () => void } }
}

const parameters = new URLSearchParams(location.search);
const scenario = (parameters.get('scenario') ?? sessionStorage.getItem('tenantFixtureScenario') ?? 'default') as Scenario;
sessionStorage.setItem('tenantFixtureScenario', scenario);
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
    if (scenario === 'management-denied') return json({ error: { message: 'Tenant management denied' } }, 403);
    if (scenario === 'management-error') return json({ error: { message: 'Tenant management temporarily unavailable' } }, 503);
    if (method === 'GET' && window.tenantFixture.holdNext) {
      window.tenantFixture.holdNext = false;
      return new Promise<Response>(resolve => { window.tenantFixture.release = () => resolve(json(tenantRecords)); });
    }
    if (scenario === 'slow' && method === 'GET') return new Promise<Response>(resolve => { window.tenantFixture.release = () => resolve(json(tenantRecords)); });
    if (scenario === 'empty' && method === 'GET') return json([]);
    if (method === 'GET') return json(tenantRecords);
    if (method === 'POST') {
      const body = JSON.parse(String(init?.body ?? '{}')) as { external_id?: string };
      const created = { external_id: body.external_id?.trim() || 'unnamed', status: 'active' as const };
      tenantRecords = [...tenantRecords, created];
      return json(created);
    }
  }
  if (url.pathname === '/internal/v1/service-tokens' && scenario === 'services-denied') return json({ error: { message: 'Service credential access denied' } }, 403);
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
  const { route, context, navigate } = useAppLocation();
  if (!isOperatorRouteKey(route) && !isPluginRouteKey(route)) return null;
  return <AppShell surface="operator" route={route} onNavigate={navigate}>
    <Operator route={route} navigationContext={context} onRouteChange={navigate} embedded showNavigation={false} />
  </AppShell>;
}

localStorage.setItem('mtc.operator.service-credential.v1', fixtureCredential);
createRoot(document.getElementById('root')!).render(<I18nProvider><MtcFluentProvider><Fixture /></MtcFluentProvider></I18nProvider>);
