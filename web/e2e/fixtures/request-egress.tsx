import { useState } from 'react';
import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system';
import { ProxyGroupsPage } from '../../src/operator/pages/ProxyGroupsPage';
import { egressSummaries } from '../support/request-egress-fixtures';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/operator.css';

window.egressFixture = { mode: 'ready', reads: [], switchTenant: () => {} };
const json = (value: unknown, status = 200) => new Response(JSON.stringify(value), { status, headers: { 'Content-Type': 'application/json' } });
window.fetch = async input => {
  const url = new URL(String(input), location.origin);
  const tenant = url.searchParams.get('tenant_external_id');
  if (url.pathname.endsWith('/access')) return json({ can_manage: true });
  if (url.pathname === '/internal/v1/upstreams') return json([]);
  if (url.pathname === '/internal/v1/transport-proxy-groups') return json({ items: tenant === 'tenant-b' ? [] : [{
    id: 'group-a', tenant_external_id: tenant, name: 'Research now', version: 3, bound_account_count: 0,
    members: [{ id: 'member-a', label: 'Current exit', scheme: 'socks5h', host: 'current.proxy.test', port: 1080, remote_dns: true, has_auth: true }],
  }] });
  if (url.pathname.endsWith('/egress-summary')) {
    window.egressFixture.reads.push(url.search);
    if (window.egressFixture.mode === 'denied') return json({ error: { message: 'credential-canary-must-not-render' } }, 403);
    if (window.egressFixture.mode === 'failed') return json({ error: { message: 'credential-canary-must-not-render' } }, 503);
    const result = { start: Number(url.searchParams.get('start')), end: Number(url.searchParams.get('end')),
      coverage: 'recorded_codex_attempts_only', measurement_basis: 'application_payload_estimate', wire_bytes: null, supplier_billed_bytes: null,
      groups: tenant === 'tenant-b' || window.egressFixture.mode === 'empty' ? [] : egressSummaries };
    if (window.egressFixture.mode === 'slow') return new Promise<Response>(resolve => { window.egressFixture.release = () => resolve(json(result)); });
    return json(result);
  }
  return json([]);
};

function Fixture() {
  const [tenant, setTenant] = useState('tenant-a');
  window.egressFixture.switchTenant = () => setTenant('tenant-b');
  return <ProxyGroupsPage token="fixture-token" tenant={tenant} onBack={() => {}} />;
}

createRoot(document.getElementById('root')!).render(<I18nProvider><MtcFluentProvider><Fixture /></MtcFluentProvider></I18nProvider>);
