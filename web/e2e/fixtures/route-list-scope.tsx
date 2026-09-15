import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system';
import { RoutesPage } from '../../src/operator/pages/ManagementPages';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/operator.css';

declare global { interface Window { routeListWrites: number } }
window.routeListWrites = 0;
const accounts = ['personal', 'team'].map(id => ({ id, tenant_external_id: 'fixture', name: id === 'personal' ? 'Kimi personal account' : 'Kimi team account', driver: 'kimi-oauth', status: 'active', config: {}, credential_generation: 1, created_at: 1, updated_at: 1 }));
const groups = [
  { id: 'kimi-group', tenant_external_id: 'fixture', name: 'Kimi 模型组', member_ids: ['personal', 'team'], member_count: 2, created_at: 1, updated_at: 1 },
  { id: 'excluded-group', tenant_external_id: 'fixture', name: '团队排除组', member_ids: ['team'], member_count: 1, created_at: 1, updated_at: 1 },
];
const common = { tenant_external_id: 'fixture', upstream_model: 'kimi-k2.5', protocol: 'openai', enabled: true, priority: 1, grant_revision: 1, created_at: 1, updated_at: 1, upstream_account_ids: [], route_group_ids: [] };
const routes = [
  { ...common, id: 'group-only', public_model: 'Kimi group-only', included_provider_group_ids: ['kimi-group'], candidate_upstream_account_ids: ['personal', 'team'] },
  { ...common, id: 'mixed', public_model: 'Kimi mixed', upstream_account_ids: ['personal'], included_provider_group_ids: ['kimi-group'], excluded_provider_group_ids: ['excluded-group'], candidate_upstream_account_ids: ['personal'] },
  { ...common, id: 'empty', public_model: 'Kimi empty', included_provider_group_ids: ['kimi-group'], excluded_provider_group_ids: ['kimi-group'], candidate_upstream_account_ids: [] },
  { ...common, id: 'unknown', public_model: 'Kimi unknown', included_provider_group_ids: ['missing-group'] },
  { ...common, id: 'direct', public_model: 'Kimi direct', upstream_account_ids: ['personal'], candidate_upstream_account_ids: ['personal'] },
];
window.fetch = async (input, init) => {
  const url = new URL(typeof input === 'string' ? input : input instanceof URL ? input.href : input.url, location.origin);
  const method = init?.method ?? (input instanceof Request ? input.method : 'GET');
  if (method !== 'GET') { window.routeListWrites++; throw new Error('Route list fixture is read-only'); }
  const values: Record<string, unknown> = { '/internal/v1/upstreams': accounts, '/internal/v1/provider-types': [{ id: 'kimi-oauth', display_name: 'Kimi Coding', protocols: ['openai'] }], '/internal/v1/model-routes': routes, '/internal/v1/provider-groups': groups, '/internal/v1/route-groups': [], '/internal/v1/plugins/group-routing-strategies': [] };
  if (!(url.pathname in values)) throw new Error(`Unexpected route list read: ${url.pathname}`);
  return new Response(JSON.stringify(values[url.pathname]), { headers: { 'Content-Type': 'application/json' } });
};
createRoot(document.getElementById('root')!).render(<I18nProvider><MtcFluentProvider><main style={{ padding: 16 }}><RoutesPage token="mock" tenant="fixture" /></main></MtcFluentProvider></I18nProvider>);
