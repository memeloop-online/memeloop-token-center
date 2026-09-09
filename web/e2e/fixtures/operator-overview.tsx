import { createRoot } from 'react-dom/client';
import { useState } from 'react';

import { Shell } from '../../src/components';
import { I18nProvider } from '../../src/i18n';
import { OverviewPage } from '../../src/operator/pages/OperatorPages';
import type { OperatorMonitoringSnapshot, OperatorUsageAnalysis, RequestView, TypedFilterAst, UsageAnalysisMetrics } from '../../src/types';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/styles/metrics.css';
import '../../src/styles/request-table.css';
import '../../src/operator/operator.css';

type FixtureTenant = 'tenant-alpha' | 'tenant-beta';

declare global {
  interface Window {
    overviewFixture: {
      calls: string[];
      delayedTenantResponsePending: boolean;
      drilldowns: TypedFilterAst[];
      releaseDelayedTenantResponse: () => void;
    };
  }
}

const alpha: FixtureTenant = 'tenant-alpha';
const beta: FixtureTenant = 'tenant-beta';
const now = Date.UTC(2026, 8, 8, 12, 0, 0);

function json(value: unknown, status = 200) {
  return new Response(JSON.stringify(value), { status, headers: { 'Content-Type': 'application/json' } });
}

function metrics(requests: number, success: number, failed: number): UsageAnalysisMetrics {
  return {
    requests, success, failed, input_tokens: requests * 12, output_tokens: requests * 4,
    cached_input_tokens: requests * 2, cache_write_tokens: requests, generation_units: 0,
    avg_duration_ms: 180, p95_duration_ms: 420, costs: [{ currency: 'USD', cost: (requests / 1000).toFixed(3) }],
  };
}

function usageAnalysis(tenant: FixtureTenant): OperatorUsageAnalysis {
  const start = now - 2 * 3_600_000;
  const points = tenant === alpha
    ? [metrics(8, 7, 1), metrics(11, 10, 1), metrics(13, 11, 2)]
    : [metrics(3, 3, 0), metrics(5, 4, 1), metrics(9, 8, 1)];
  return {
    from_created_at: now - 86_400_000, to_created_at: now, granularity: 'hour', time_zone: 'UTC',
    p95_is_approximate: true, p95_method: 'fixture_histogram', upstream_grouping: 'stable_account',
    summary: metrics(points.reduce((total, point) => total + point.requests, 0), points.reduce((total, point) => total + point.success, 0), points.reduce((total, point) => total + point.failed, 0)),
    time_series: points.map((point, index) => ({ ...point, bucket_start: start + index * 3_600_000 })),
    by_model: [], by_key: [], by_session: [], by_upstream: [], by_protocol: [], by_status: [], errors: [], heatmap: [],
  };
}

function monitoring(tenant: FixtureTenant): OperatorMonitoringSnapshot {
  const summary = tenant === alpha ? { requests: 32, successful_requests: 28, failed_requests: 4 } : { requests: 17, successful_requests: 15, failed_requests: 2 };
  return {
    contract_version: 'v1', generated_at: now, scope: 'tenant', tenant_external_id: tenant,
    from_created_at: now - 86_400_000, to_created_at: now, granularity: 'hour',
    latency_is_approximate: true, latency_method: 'fixed_histogram_upper_bound_capped_60000ms',
    summary: { ...summary, avg_duration_ms: 180, p95_duration_ms: 420, costs: [{ currency: 'USD', cost: '0.017' }] },
    freshness: { latest_terminal_created_at: now, age_millis: 0 },
    health: { version: 'upstream_breaker_v1', status: 'unknown', observed_at: now },
    top_upstream_models: [],
  };
}

function requests(tenant: FixtureTenant): RequestView[] {
  return [{
    request_id: tenant === alpha ? 'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa' : 'bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb',
    created_at: now, completed_at: now + 180, protocol: 'openai',
    model: tenant === alpha ? 'alpha-stale-model' : 'beta-current-model', status_code: 200, duration_ms: 180,
    input_tokens: 12, cached_input_tokens: 2, cache_write_tokens: 1, output_tokens: 4, cost: '0.001', currency: 'USD', error_code: null,
  }];
}

let releaseDelayedTenantResponse: (() => void) | undefined;
window.overviewFixture = {
  calls: [],
  delayedTenantResponsePending: false,
  drilldowns: [],
  releaseDelayedTenantResponse: () => {
    releaseDelayedTenantResponse?.();
    releaseDelayedTenantResponse = undefined;
    window.overviewFixture.delayedTenantResponsePending = false;
  },
};

globalThis.fetch = async (input: RequestInfo | URL) => {
  const source = typeof input === 'string' ? input : input instanceof URL ? input.toString() : input.url;
  const url = new URL(source, location.origin);
  const tenant = url.searchParams.get('tenant_external_id') as FixtureTenant | null;
  window.overviewFixture.calls.push(`GET ${url.pathname}${url.search}`);
  if (!tenant || (tenant !== alpha && tenant !== beta)) return json({ error: { message: 'unexpected fixture scope' } }, 400);
  if (url.pathname === '/internal/v1/requests') {
    if (tenant === alpha) {
      window.overviewFixture.delayedTenantResponsePending = true;
      return new Promise<Response>((resolve) => { releaseDelayedTenantResponse = () => resolve(json(requests(alpha))); });
    }
    return json(requests(beta));
  }
  if (url.pathname === '/internal/v1/monitoring-snapshot') {
    if (tenant === alpha) return json({ error: { message: 'Monitoring fixture unavailable' } }, 503);
    return json(monitoring(beta));
  }
  if (url.pathname === '/internal/v1/usage-analysis') return json(usageAnalysis(tenant));
  return json({ error: { message: 'unexpected fixture endpoint' } }, 404);
};

function Fixture() {
  const [tenant, setTenant] = useState<FixtureTenant>(alpha);
  return <Shell operator>
    <div className="hero compact">
      <div><span className="eyebrow">Fixture</span><h1>Operator overview</h1></div>
      <button type="button" className="secondary" data-fixture-tenant-switch onClick={() => setTenant((current) => current === alpha ? beta : alpha)}>Switch tenant</button>
    </div>
    <output data-fixture-tenant={tenant}>{tenant}</output>
    <OverviewPage token="mts_overview_fixture" tenant={tenant} onNavigate={() => undefined} onRequestDrilldown={(ast) => window.overviewFixture.drilldowns.push(ast)} onOpenUsageSession={() => undefined} onOpenSession={() => undefined} />
  </Shell>;
}

createRoot(document.getElementById('root')!).render(<I18nProvider><Fixture /></I18nProvider>);
