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

// outage: the baseline tenant-scope read fails with a transport-style 503.
// forbidden: the baseline tenant-scope read answers a definitive 403.
// management-denied: only the tenant-management API answers 403; the
// baseline tenant-scope read stays healthy.
type Mode = 'ok' | 'outage' | 'forbidden' | 'management-denied';

interface TenantRecord {
  external_id: string;
  status: 'active' | 'archived';
}

declare global {
  interface Window { degradedFixture: { calls: string[]; mode: Mode } }
}

const parameters = new URLSearchParams(location.search);
const fixtureCredential = 'mts_degraded_fixture_alpha';
let tenantRecords: TenantRecord[] = [
  { external_id: 'default', status: 'active' },
  { external_id: 'north', status: 'active' },
];

window.degradedFixture = { calls: [], mode: 'ok' };

function json(value: unknown, status = 200) {
  return new Response(JSON.stringify(value), {
    status,
    headers: { 'Content-Type': 'application/json' },
  });
}

function credentialOf(init?: RequestInit) {
  const header = new Headers(init?.headers).get('Authorization') ?? '';
  return header.replace(/^Bearer\s+/, '');
}

function credentialTag(credential: string) {
  return credential ? credential.slice(-5) : '-';
}

function managementTenant(pathname: string) {
  return decodeURIComponent(pathname.slice('/internal/v1/tenant-management/'.length).split('/')[0] ?? '');
}

globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
  const url = new URL(typeof input === 'string' ? input : input.toString(), location.origin);
  const method = init?.method ?? 'GET';
  const credential = credentialOf(init);
  window.degradedFixture.calls.push(`${method} ${url.pathname} [${credentialTag(credential)}]`);
  if (url.pathname === '/internal/v1/tenants') {
    if (credential !== fixtureCredential) return json({ error: { message: 'Credential rejected' } }, 401);
    if (window.degradedFixture.mode === 'outage') return json({ error: { message: 'Tenant store unavailable' } }, 503);
    if (window.degradedFixture.mode === 'forbidden') return json({ error: { message: 'Operator role revoked' } }, 403);
    return json(tenantRecords.filter((tenant) => tenant.status === 'active').map(({ external_id }) => ({ external_id })));
  }
  if (url.pathname === '/internal/v1/tenant-management') {
    if (credential !== fixtureCredential) return json({ error: { message: 'Credential rejected' } }, 401);
    if (window.degradedFixture.mode === 'management-denied') return json({ error: { message: 'Tenant management scope missing' } }, 403);
    if (method === 'GET') return json(tenantRecords);
    if (method === 'POST') {
      const body = JSON.parse(String(init?.body ?? '{}')) as { external_id?: string };
      const created = { external_id: body.external_id?.trim() || 'unnamed', status: 'active' as const };
      tenantRecords = [...tenantRecords, created];
      return json(created);
    }
  }
  if (url.pathname.startsWith('/internal/v1/tenant-management/')) {
    if (credential !== fixtureCredential) return json({ error: { message: 'Credential rejected' } }, 401);
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
  if (url.pathname === '/internal/v1/plugins') return json([]);
  if (url.pathname === '/internal/v1/model-picker-options') return json({ contract_version: 'model_picker_projection_v1', data: [], next_cursor: null });
  if (url.pathname === '/internal/v1/filter-assistant/settings' && method === 'GET') return json(null);
  return json([]);
};

function Fixture() {
  const { route, context, navigate } = useAppLocation();
  if (!isOperatorRouteKey(route) && !isPluginRouteKey(route)) return null;
  return <AppShell surface="operator" route={route} onNavigate={navigate}>
    <Operator route={route} navigationContext={context} onRouteChange={navigate} embedded showNavigation={false} />
  </AppShell>;
}

if (parameters.get('seeded') !== '0') localStorage.setItem('mtc.operator.service-credential.v1', fixtureCredential);
createRoot(document.getElementById('root')!).render(<I18nProvider><MtcFluentProvider><Fixture /></MtcFluentProvider></I18nProvider>);
