import { createRoot } from 'react-dom/client';
import { useEffect, useState } from 'react';

import { I18nProvider } from '../../src/i18n';
import { Operator } from '../../src/operator/Operator';
import type { OperatorMonitoringSnapshot, OperatorUsageAnalysis, OperatorUsageAnalysisTrends, RequestDetail, RequestView, UsageAnalysisMetrics } from '../../src/types';
import type { OperatorRouteKey } from '../../src/operator/scope/operatorRoutes';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/styles/metrics.css';
import '../../src/styles/request-table.css';
import '../../src/operator/operator.css';

declare global {
  interface Window {
    overviewDrilldownFixture: {
      calls: string[];
      requestQueryBodies: unknown[];
      route: OperatorRouteKey;
      openOverview: () => void;
    };
  }
}

const credential = 'mts_overview_drilldown_fixture';
const tenant = 'drilldown-tenant';
const now = Date.UTC(2026, 8, 8, 12, 0, 0);
const bucketStart = now - 3_600_000;
const usageSessions = new URLSearchParams(location.search).has('usage-sessions');
const requestSession = new URLSearchParams(location.search).has('request-session');
const initialSessions = new URLSearchParams(location.search).has('initial-sessions');
const sessionKey = '5dae9b5a-6bd0-54a7-ae14-fb5aa4f61650';
const realSession = '00000000-0000-4000-8000-000000000002';

window.overviewDrilldownFixture = { calls: [], requestQueryBodies: [], route: 'overview', openOverview: () => undefined };

function json(value: unknown, status = 200) {
  return new Response(JSON.stringify(value), { status, headers: { 'Content-Type': 'application/json' } });
}

function metrics(): UsageAnalysisMetrics {
  return {
    requests: 7, success: 6, failed: 1, input_tokens: 84, output_tokens: 28,
    cached_input_tokens: 14, cache_write_tokens: 7, generation_units: 0,
    avg_duration_ms: 180, p95_duration_ms: 420, costs: [{ currency: 'USD', cost: '0.007' }],
  };
}

const usageAnalysis: OperatorUsageAnalysis = {
  from_created_at: bucketStart, to_created_at: now, granularity: 'hour', time_zone: 'UTC',
  p95_is_approximate: true, p95_method: 'fixture_histogram', upstream_grouping: 'stable_account',
  summary: metrics(), time_series: [{ ...metrics(), bucket_start: bucketStart }],
  by_model: [], by_key: [], by_session: usageSessions ? [
    { ...metrics(), id: `unlinked:${sessionKey}`, label: 'Codex credential', key_id: sessionKey, key_alias: 'Codex credential', unlinked: true },
    { ...metrics(), id: realSession, label: 'Named Codex conversation', key_id: sessionKey, key_alias: 'Codex credential', unlinked: false },
  ] : [], by_upstream: [], by_protocol: [], by_status: [], errors: [], heatmap: [],
};

const usageTrends: OperatorUsageAnalysisTrends = {
  from_created_at: usageAnalysis.from_created_at,
  to_created_at: usageAnalysis.to_created_at,
  granularity: usageAnalysis.granularity,
  time_zone: usageAnalysis.time_zone,
  p95_is_approximate: true,
  p95_method: 'fixed_histogram_upper_bound_capped_60000ms',
  summary: usageAnalysis.summary,
  time_series: usageAnalysis.time_series,
};

const monitoring: OperatorMonitoringSnapshot = {
  contract_version: 'v1', generated_at: now, scope: 'tenant', tenant_external_id: tenant,
  from_created_at: bucketStart, to_created_at: now, granularity: 'hour',
  latency_is_approximate: true, latency_method: 'fixed_histogram_upper_bound_capped_60000ms',
  summary: { requests: 7, successful_requests: 6, failed_requests: 1, avg_duration_ms: 180, p95_duration_ms: 420, costs: [{ currency: 'USD', cost: '0.007' }] },
  freshness: { latest_terminal_created_at: now, age_millis: 0 },
  health: { version: 'upstream_breaker_v1', status: 'unknown', observed_at: now },
  top_upstream_models: [],
};

const drilledRequest: RequestView = {
  request_id: 'dddddddd-dddd-4ddd-8ddd-dddddddddddd', created_at: bucketStart + 1000, completed_at: bucketStart + 1180,
  protocol: 'openai', model: 'drilled-model', status_code: 200, duration_ms: 180,
  input_tokens: 12, cached_input_tokens: 2, cache_write_tokens: 1, output_tokens: 4,
  cost: '0.001', currency: 'USD', error_code: null,
  credential_identity: { tenant_external_id: tenant, key_id: sessionKey, key_alias: 'Codex credential', principal_external_id: 'Codex user' },
  ...(requestSession ? { session_context: { session_id: realSession, association: 'confirmed' as const, session_name: 'Named Codex conversation', task_kind: 'interactive', agent_id: 'codex', semantics_source: 'declared' } } : {}),
};
const drilledRequestDetail: RequestDetail = { ...drilledRequest, request_body: { model: drilledRequest.model }, response_body: { id: 'fixture-response' }, archive_complete: true };

globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
  const source = typeof input === 'string' ? input : input instanceof URL ? input.toString() : input.url;
  const url = new URL(source, location.origin);
  const method = init?.method ?? (input instanceof Request ? input.method : 'GET');
  window.overviewDrilldownFixture.calls.push(`${method} ${url.pathname}${url.search}`);
  if (url.pathname === '/internal/v1/tenants') return json([{ external_id: tenant }]);
  if (url.pathname === '/internal/v1/plugins') return json([]);
  if (url.pathname === '/internal/v1/usage-analysis/trends') return json(usageTrends);
  if (url.pathname === '/internal/v1/usage-analysis') return json(usageAnalysis);
  if (url.pathname === '/internal/v1/monitoring-snapshot') return json(monitoring);
  if (url.pathname === '/internal/v1/requests' && method === 'GET') return json([drilledRequest]);
  if (url.pathname === `/internal/v1/requests/${drilledRequest.request_id}` && method === 'GET') return json(drilledRequestDetail);
  if (url.pathname === '/internal/v1/upstreams') return json([]);
  if (url.pathname === '/internal/v1/keys') return json([]);
  if (url.pathname === '/internal/v1/filter-presets') return json({ named: [], recent: [] });
  if (url.pathname === '/internal/v1/sessions' || url.pathname === '/internal/v1/sessions/summaries') return json({
    generated_at: now, next_cursor: null,
    sessions: [...(initialSessions && url.pathname === '/internal/v1/sessions' ? [{
      session_id: `unlinked:${sessionKey}`, session_name: null, cluster_id: null,
      key_id: sessionKey, key_alias: 'Codex credential', unlinked: true,
      last_activity_at: now, model: 'drilled-model', protocol: 'openai', last_status: 'success',
      active_requests: 0, requests: 7, errors: 0, input_tokens: 84, output_tokens: 28, costs: [],
      avg_duration_ms: 180, archived_only_requests: 0, archived_only_errors: 0,
      archived_only_input_tokens: 0, archived_only_output_tokens: 0, archived_only_avg_duration_ms: null,
    }] : []), {
      session_id: realSession, session_name: 'Named Codex conversation', cluster_id: realSession,
      key_id: sessionKey, key_alias: 'Codex credential', unlinked: false,
      last_activity_at: now, model: 'drilled-model', protocol: 'openai', last_status: 'success',
      active_requests: 0, requests: 7, errors: 0, input_tokens: 84, output_tokens: 28, costs: [],
      avg_duration_ms: 180, archived_only_requests: 0, archived_only_errors: 0,
      archived_only_input_tokens: 0, archived_only_output_tokens: 0, archived_only_avg_duration_ms: null,
    }],
  });
  if (url.pathname === `/internal/v1/sessions/${realSession}`) return json({
    session_id: realSession, cluster_id: realSession, unlinked: false,
    requests: [{ ...drilledRequest, source: 'live', provenance: 'native', unlinked: false,
      session_context: { session_id: realSession, association: 'confirmed', session_name: 'Named Codex conversation', task_kind: 'interactive', agent_id: 'codex', semantics_source: 'declared' } }],
    edges: [], has_more: false, next_cursor: null, edges_truncated: false,
  });
  if (url.pathname === `/internal/v1/sessions/${realSession}/relationships`) return json({ relationships: [] });
  if (url.pathname === `/internal/v1/sessions/${realSession}/title`) return json({ title: 'Named Codex conversation', state: 'ready', error_code: null, updated_at: now, source: 'manual', revision: 1 });
  if (url.pathname === '/internal/v1/requests/query' && method === 'POST') {
    const body = JSON.parse(String(init?.body ?? '{}'));
    window.overviewDrilldownFixture.requestQueryBodies.push(body);
    const filtered = Array.isArray(body.ast?.conditions) && body.ast.conditions.some((condition: { field: string }) => condition.field === 'created_at');
    return json({ requests: filtered ? [drilledRequest] : [], next_cursor: null });
  }
  return json({ error: { message: `Unexpected fixture endpoint: ${url.pathname}` } }, 404);
};

function Fixture() {
  const [route, setRoute] = useState<OperatorRouteKey>(initialSessions ? 'sessions' : usageSessions ? 'usage' : 'overview');
  window.overviewDrilldownFixture.openOverview = () => setRoute(usageSessions ? 'usage' : 'overview');
  useEffect(() => { window.overviewDrilldownFixture.route = route; }, [route]);
  return <>
    <output data-fixture-route={route}>{route}</output>
    <Operator route={route} onRouteChange={setRoute} embedded showNavigation={false} />
  </>;
}

localStorage.setItem('mtc.operator.service-credential.v1', credential);
createRoot(document.getElementById('root')!).render(<I18nProvider><Fixture /></I18nProvider>);
