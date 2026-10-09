import { createRoot } from 'react-dom/client';
import { AppShell } from '../../src/app/AppShell';
import { useAppLocation } from '../../src/app/useAppLocation';
import { isPluginRouteKey } from '../../src/app/routes';
import { isOperatorRouteKey } from '../../src/operator/scope/operatorRoutes';
import { I18nProvider } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system';
import { Operator } from '../../src/operator/Operator';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/app-shell.css';

declare global {
  interface Window { releaseNavigationProxyAccess?: () => void }
}

const account = {
  id: 'account-native', tenant_external_id: 'fixture', name: '研发订阅', driver: 'openai-codex', auth_kind: 'oauth', connection_method: 'oauth',
  status: 'active', config: { base_url: 'https://chatgpt.com/backend-api/codex' }, has_proxy: true, proxy_scheme: 'socks5h',
  proxy_remote_dns: true, can_update_transport_proxy: true, credential_generation: 7, route_count: 0, updated_at: 100,
};
const slowAccess = new URL(location.href).searchParams.get('access') === 'slow';

globalThis.fetch = async input => {
  const url = new URL(typeof input === 'string' ? input : input instanceof URL ? input.href : input.url, location.origin);
  let value: unknown = [];
  if (url.pathname === '/internal/v1/tenants') value = [{ external_id: 'fixture' }];
  if (url.pathname === '/internal/v1/tenant-management') value = [{ external_id: 'fixture', status: 'active' }];
  if (url.pathname === '/internal/v1/transport-proxy-groups/access') {
    if (slowAccess) return new Promise<Response>(resolve => {
      window.releaseNavigationProxyAccess = () => resolve(new Response(JSON.stringify({ can_manage: false })));
    });
    value = { can_manage: false };
  }
  if (url.pathname === '/internal/v1/upstreams') value = [account];
  if (url.pathname === '/internal/v1/provider-types') value = [{ id: 'openai-codex', display_name: 'Codex', source: 'builtin', protocols: ['openai'], credential_schema: { type: 'object', properties: {} }, config_schema: { type: 'object', properties: {} } }];
  if (url.pathname.includes('monitoring')) value = { contract_version: 'v1', top_upstream_models: [] };
  if (url.pathname.includes('availability')) value = { contract_version: 'upstream_account_availability_v1', accounts: [] };
  return new Response(JSON.stringify(value), { headers: { 'Content-Type': 'application/json' } });
};

function Fixture() {
  const { route, context, navigate } = useAppLocation();
  if (!isOperatorRouteKey(route) && !isPluginRouteKey(route)) return null;
  return <AppShell surface="operator" route={route} onNavigate={navigate}>
    <Operator route={route} navigationContext={context} onRouteChange={navigate} embedded showNavigation={false} />
  </AppShell>;
}

localStorage.setItem('mtc.operator.service-credential.v1', 'mts_navigation_fixture');
createRoot(document.getElementById('root')!).render(<I18nProvider><MtcFluentProvider><Fixture /></MtcFluentProvider></I18nProvider>);
