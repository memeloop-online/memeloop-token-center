import { createRoot } from 'react-dom/client';
import { useState } from 'react';
import { I18nProvider } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system';
import { ProvidersPage, RoutesPage } from '../../src/operator/pages/ManagementPages';
import { AppShell } from '../../src/app/AppShell';
import type { AppRouteKey } from '../../src/app/routes';
import type { UpstreamQuotaSnapshot } from '../../src/operator/upstreamQuota';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/operator.css';
import '../../src/app-shell.css';

import type {} from '../support/form-journey-globals';
import { providerEditShape } from './provider-edit-shapes';
window.formJourneyReads = []; window.formJourneyWrites = 0;
window.failNextFormWrite = false;
window.deferNextFormQuotaRead = false;
window.deferNextFormProxyRead = false;
let releaseProxy: (() => void) | undefined;
window.releaseFormProxyRead = () => { if (!releaseProxy) throw new Error('No pending proxy read'); releaseProxy(); releaseProxy = undefined; };
let releaseQuota: (() => void) | undefined;
window.releaseFormQuotaRead = () => { if (!releaseQuota) throw new Error('No pending quota read'); releaseQuota(); releaseQuota = undefined; };
const workflows = new URLSearchParams(location.search).has('workflows');
const existingRoute = { id: 'route-existing', tenant_external_id: 'fixture', public_model: 'research-model', upstream_model: 'fixture-model', protocol: 'openai', upstream_account_ids: ['account-native'], enabled: true, priority: 0, grant_revision: 1, created_at: 1, updated_at: 1 };
let routeRows = [existingRoute];
const account = { id: 'account-native', tenant_external_id: 'fixture', name: '研发订阅', driver: 'openai-codex', auth_kind: 'oauth', connection_method: 'oauth', status: 'active', config: { base_url: 'https://chatgpt.com/backend-api/codex' }, has_proxy: true, proxy_scheme: 'socks5h', proxy_remote_dns: true, can_update_transport_proxy: !new URLSearchParams(location.search).has('proxy-no-authority'), credential_generation: 1, route_count: 1, updated_at: 1 };
const editShape = providerEditShape(new URLSearchParams(location.search).get('provider-shape'));
if (editShape) { Object.assign(account.config, editShape.config); Object.assign(account, { name: 'synthetic.automation.account@example.invalid', proxy_fingerprint: 'synthetic-diagnostic-fingerprint' }); }
let proxyUrl = 'socks5h://fixture-user:fixture-password@10.0.0.15:1080';
window.fetch = async (input, init) => {
  const path = new URL(typeof input === 'string' ? input : input instanceof URL ? input.href : input.url, location.origin).pathname;
  const method = init?.method ?? (input instanceof Request ? input.method : 'GET');
  if (method !== 'GET') {
    window.formJourneyWrites++;
    if (workflows && new URLSearchParams(location.search).has('provider-workflow') && method === 'POST' && path === '/internal/v1/upstreams') {
      if (window.failNextFormWrite) { window.failNextFormWrite = false; return new Response(JSON.stringify({ error: { message: '模拟创建失败，草稿仍在' } }), { status: 400 }); }
      return new Response(JSON.stringify({ ...account, id: 'account-created', name: '已创建测试上游' }), { status: 201 });
    }
    if (workflows && new URLSearchParams(location.search).has('proxy-workflow') && method === 'PUT' &&
      (path === '/internal/v1/upstreams/account-native/transport-proxy' || path === '/internal/v1/upstreams/account-native')) {
      const data = JSON.parse(String(init?.body ?? '{}'));
      if (data.expected_updated_at !== account.updated_at) return new Response(JSON.stringify({ error: { message: 'fixture revision conflict' } }), { status: 409 });
      if (path.endsWith('/transport-proxy')) {
        if (data.expected_credential_generation !== account.credential_generation) throw new Error('fixture credential revision mismatch');
        account.credential_generation++;
        proxyUrl = data.proxy_url;
      } else {
        window.formJourneyLastProviderWrite = structuredClone(data);
        account.name = data.name;
        account.config = data.config;
      }
      account.updated_at++;
      return new Response(JSON.stringify(account));
    }
    if (!workflows || !['/internal/v1/model-routes', '/internal/v1/model-routes/route-existing'].includes(path) || !['POST', 'PUT'].includes(method)) throw new Error('Mutation outside the explicit local workflow fixture');
    if (window.failNextFormWrite) { window.failNextFormWrite = false; return new Response(JSON.stringify({ error: { message: '模拟保存失败，草稿仍在' } }), { status: 400 }); }
    const data = JSON.parse(String(init?.body ?? '{}'));
    if (method === 'POST') routeRows = [...routeRows, { ...existingRoute, ...data, id: 'route-created' }];
    else routeRows = routeRows.map(route => route.id === 'route-existing' ? { ...route, ...data } : route);
    return new Response(JSON.stringify({ ...existingRoute, ...data }));
  }
  window.formJourneyReads.push(path);
  if (path === '/internal/v1/upstreams/account-native/transport-proxy') {
    if (init?.cache !== 'no-store') throw new Error('Proxy reads must not be cached');
    const snapshot = JSON.stringify({ account_id: account.id, proxy_url: proxyUrl, supported: true, proxy_network_scope: 'private', updated_at: account.updated_at, credential_generation: account.credential_generation });
    if (window.deferNextFormProxyRead) {
      window.deferNextFormProxyRead = false;
      // Ignore AbortSignal to prove a late response cannot re-publish a prior
      // credential generation's original URL after a successful save.
      return new Promise<Response>(resolve => { releaseProxy = () => resolve(new Response(snapshot)); });
    }
    return new Response(snapshot, { headers: { 'Cache-Control': 'private, no-store' } });
  }
  if (new URLSearchParams(location.search).has('quota-generation') && path === '/internal/v1/upstreams/account-native/quota') {
    const generation = account.credential_generation;
    const now = Date.now();
    const snapshot: UpstreamQuotaSnapshot = {
      contract_version: 'upstream_quota_v1', upstream_account_id: account.id, tenant_external_id: 'fixture', provider: account.driver,
      status: 'ready', observed_at: now, stale_after: null, stale: false, freshness: 'fresh', plan_type: null, workspace: null, error_code: null,
      capabilities: { read: true, plan: true, workspace: false, window_amounts: false, window_amount_unit: false, window_percent: true, reset_credit_expiry: true, subscription_expiry: false, supplier_read_only: true, refreshes_credentials: false, consumes_reset_credit: false },
      subscription_active_until: null, credits: { balance: null, unlimited: null, has_credits: null, source: null },
      reset_credits: [{ status: 'available', granted_at: now - 60_000, expires_at: now + 3_600_000, source: 'codex_reset_credits' }],
      windows: [{ id: 'primary', label: `代次 ${generation} 额度`, used_percent: generation === 1 ? 75 : 25, used: null, remaining: null, limit: null, unit: null, reset_at: now + 3_600_000, period_seconds: null, source: 'provider_usage', reset_is_estimated: false, allowed: true, limit_reached: false }],
      reset_capability: { provider_supported: generation === 1, implementation_available: generation === 1, prepare_available: generation === 1, confirmation_required: true, retryable: false, available_credits: 1, applicable_credits: 1, reason: generation === 1 ? 'explicit_confirmation_required' : 'quota_reset_not_supported', credit_error_code: null, evidence: 'server_driver_contract' },
    };
    if (window.deferNextFormQuotaRead) {
      window.deferNextFormQuotaRead = false;
      // Deliberately ignore AbortSignal, and preserve the old generation's
      // response, so the component must fence a late result itself.
      return new Promise<Response>(resolve => { releaseQuota = () => resolve(new Response(JSON.stringify(snapshot))); });
    }
    return new Response(JSON.stringify(snapshot));
  }
  const providers = [{ id: 'openai-codex', display_name: 'Codex 订阅', source: 'builtin', protocols: ['openai'],
    credential_schema: { type: 'object', properties: { type: { const: 'oauth' } } }, config_schema: { type: 'object', properties: { base_url: { type: 'string', const: 'https://chatgpt.com/backend-api/codex', readOnly: true } } }, oauth_adapter: { flow_kind: 'openai_device' } },
  { id: 'http-json', display_name: '自部署模型', source: 'builtin', protocols: ['openai'], credential_schema: { type: 'object', properties: { api_key: { type: 'string', title: 'API key', writeOnly: true } } },
    config_schema: { type: 'object', required: ['base_url'], properties: { base_url: { type: 'string', title: 'Base URL' }, timeout_seconds: { type: 'integer', title: 'Timeout seconds', minimum: 1, default: 30 } } } }];
  if (editShape) {
    Object.assign(providers[0].config_schema, { required: editShape.schema.required });
    Object.assign(providers[0].config_schema.properties, editShape.schema.properties);
  }
  let value: unknown = [];
  if (path === '/internal/v1/provider-types') value = providers;
  else if (path === '/internal/v1/upstreams') value = workflows ? [account] : [];
  else if (path === '/internal/v1/model-routes') value = workflows ? routeRows : [];
  else if (path.endsWith('/models')) value = { status: 'ready', models: [{ id: 'fixture-model', protocol: 'openai' }] };
  else if (path === '/internal/v1/upstream-models') value = { data: workflows ? [{ id: 'fixture-model', protocol: 'openai', supported_account_count: 1, eligible_account_count: 1, complete_coverage: true }] : [], eligible_account_count: workflows ? 1 : 0, unknown_account_count: 0, stale_account_count: 0 };
  else if (path.includes('monitoring')) value = { contract_version: 'v1', top_upstream_models: [] };
  else if (path.includes('availability')) value = { contract_version: 'upstream_account_availability_v1', accounts: [] };
  return new Response(JSON.stringify(value), { status: 200, headers: { 'Content-Type': 'application/json' } });
};
const routes = new URLSearchParams(location.search).get('view') === 'routes';
function Preview() {
  const [view, setView] = useState<AppRouteKey>(routes ? 'routes' : 'providers');
  const content = view === 'routes' ? <RoutesPage token="mock-only" tenant="fixture" /> : <ProvidersPage token="mock-only" tenant="fixture" />;
  return workflows ? <AppShell surface="operator" route={view} onNavigate={setView}>{content}</AppShell> : <main style={{ padding: 20, maxWidth: 1100, margin: 'auto' }}>{content}</main>;
}
createRoot(document.getElementById('root')!).render(<I18nProvider><MtcFluentProvider><Preview /></MtcFluentProvider></I18nProvider>);
