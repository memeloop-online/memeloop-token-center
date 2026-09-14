import { createRoot } from 'react-dom/client';
import { useEffect, useState } from 'react';

import { I18nProvider } from '../../src/i18n';
import { Operator } from '../../src/operator/Operator';
import type { OperatorMonitoringSnapshot, OperatorUsageAnalysis, OperatorUsageAnalysisTrends, RequestView, UsageAnalysisMetrics } from '../../src/types';
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
    };
  }
}

const credential = 'mts_overview_drilldown_fixture';
const tenant = 'drilldown-tenant';
const now = Date.UTC(2026, 8, 8, 12, 0, 0);
const bucketStart = now - 3_600_000;

window.overviewDrilldownFixture = { calls: [], requestQueryBodies: [], route: 'overview' };

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
  by_model: [], by_key: [], by_session: [], by_upstream: [], by_protocol: [], by_status: [], errors: [], heatmap: [],
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
};

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
  if (url.pathname === '/internal/v1/requests' && method === 'GET') return json([]);
  if (url.pathname === '/internal/v1/upstreams') return json([]);
  if (url.pathname === '/internal/v1/requests/query' && method === 'POST') {
    const body = JSON.parse(String(init?.body ?? '{}'));
    window.overviewDrilldownFixture.requestQueryBodies.push(body);
    const filtered = Array.isArray(body.ast?.conditions) && body.ast.conditions.length === 1
      && body.ast.conditions[0]?.field === 'created_at' && body.ast.conditions[0]?.operator === 'between';
    return json({ requests: filtered ? [drilledRequest] : [], next_cursor: null });
  }
  return json({ error: { message: `Unexpected fixture endpoint: ${url.pathname}` } }, 404);
};

function Fixture() {
  const [route, setRoute] = useState<OperatorRouteKey>('overview');
  useEffect(() => { window.overviewDrilldownFixture.route = route; }, [route]);
  return <>
    <output data-fixture-route={route}>{route}</output>
    <Operator route={route} onRouteChange={setRoute} embedded showNavigation={false} />
  </>;
}

localStorage.setItem('mtc.operator.service-credential.v1', credential);
createRoot(document.getElementById('root')!).render(<I18nProvider><Fixture /></I18nProvider>);
