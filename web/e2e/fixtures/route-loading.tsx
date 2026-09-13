import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';

import { I18nProvider } from '../../src/i18n';
import { RoutesPage } from '../../src/operator/pages/ManagementPages';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/operator.css';

declare global {
  interface Window { routeLoadingFixture: { calls: string[] } }
}

window.routeLoadingFixture = { calls: [] };

function json(value: unknown, status = 200) {
  return new Response(JSON.stringify(value), { status, headers: { 'Content-Type': 'application/json' } });
}

globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
  const href = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url;
  const url = new URL(href, location.origin);
  const call = `${url.pathname}${url.search}`;
  const signal = input instanceof Request ? input.signal : init?.signal;
  // React StrictMode deliberately probes effect cleanup before the browser can
  // paint. Let that cleanup cancel its probe before recording the one live
  // request the behavior contract observes.
  await Promise.resolve();
  if (signal?.aborted) throw signal.reason ?? new DOMException('Aborted', 'AbortError');
  window.routeLoadingFixture.calls.push(call);
  if (url.pathname === '/internal/v1/provider-types' || url.pathname === '/internal/v1/upstreams') return json([]);
  if (url.pathname === '/internal/v1/provider-groups' || url.pathname === '/internal/v1/route-groups') return json([]);
  if (url.pathname === '/internal/v1/upstream-models') return json({ data: [], next_cursor: null });
  if (url.pathname === '/internal/v1/model-routes') return json([{
    id: 'route-loading-fixture', tenant_external_id: 'tenant-route-loading', public_model: 'fixture-public-model',
    upstream_account_ids: [], included_provider_group_ids: [], excluded_provider_group_ids: [], route_group_ids: [],
    granted_credential_ids: [], custom_model_confirmed: false, upstream_model: 'fixture-upstream-model', protocol: 'openai',
    priority: 0, enabled: true, created_at: 1_700_000_000_000, updated_at: 1_700_000_000_000, grant_revision: 1,
  }]);
  if (url.pathname === '/internal/v1/keys') {
    if (url.searchParams.has('key_id')) return json({ error: { message: 'fixture exact lookup unavailable' } }, 503);
    return json([]);
  }
  return json({ error: { message: `unexpected fixture request: ${call}` } }, 404);
};

createRoot(document.getElementById('root')!).render(
  <StrictMode><I18nProvider><RoutesPage token="mts_route_loading" tenant="tenant-route-loading" /></I18nProvider></StrictMode>,
);
