import { createRoot } from 'react-dom/client';

import { I18nProvider } from '../../src/i18n';
import { UpstreamAvailability } from '../../src/operator/UpstreamAvailability';
import type { OperatorMonitoringSnapshot, UpstreamAccount } from '../../src/types';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/operator.css';

const now = Date.UTC(2026, 8, 9, 12, 0, 0);

declare global {
  interface Window { upstreamAvailabilityFixture: { openedRequestId?: string } }
}

window.upstreamAvailabilityFixture = {};

const observedAccount: UpstreamAccount = {
  id: 'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa', tenant_id: 'tenant-alpha', tenant_external_id: 'tenant-alpha',
  name: 'Primary OpenAI', driver: 'openai', auth_kind: 'api_key', connection_method: 'api_key', credential_generation: 3,
  status: 'active', credential_expires_at: now + 2 * 86_400_000, can_refresh: false, can_rotate: true, can_reauthorize: false,
  route_count: 2, config: {}, created_at: now - 10 * 86_400_000, updated_at: now,
};

const unobservedAccount: UpstreamAccount = {
  ...observedAccount, id: 'bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb', name: 'Fallback account', driver: 'anthropic',
  status: 'disabled', credential_expires_at: now - 1_000, route_count: 0,
};

const snapshot: OperatorMonitoringSnapshot = {
  contract_version: 'v1', generated_at: now, scope: 'tenant', tenant_external_id: 'tenant-alpha',
  from_created_at: now - 86_400_000, to_created_at: now, granularity: 'hour',
  latency_is_approximate: true, latency_method: 'fixed_histogram_upper_bound_capped_60000ms',
  summary: { requests: 9, successful_requests: 6, failed_requests: 3, avg_duration_ms: 230, p95_duration_ms: 660, costs: [] },
  freshness: { latest_terminal_created_at: now - 5_000, age_millis: 5_000 },
  health: { version: 'upstream_breaker_v1', status: 'degraded', observed_at: now - 5_000 },
  top_upstream_models: [
    {
      upstream_account_id: observedAccount.id, upstream_name: observedAccount.name, model: 'gpt-5.4',
      metrics: { requests: 7, successful_requests: 5, failed_requests: 2, avg_duration_ms: 180, p95_duration_ms: 420, costs: [] },
      health: { version: 'upstream_breaker_v1', status: 'degraded', observed_at: now - 5_000 },
      terminal_outcomes: [
        { id: 'req-newest', source: 'request', created_at: now - 5_000, status: 'failure', duration_ms: 250, error_code: 'upstream_timeout' },
        { id: 'req-second', source: 'request', created_at: now - 25_000, status: 'success', duration_ms: 180, error_code: null },
        { id: 'gen-third', source: 'generation', created_at: now - 45_000, status: 'success', duration_ms: 210, error_code: null },
        { id: 'req-fourth', source: 'request', created_at: now - 65_000, status: 'failure', duration_ms: 620, error_code: 'rate_limited' },
        { id: 'req-fifth', source: 'request', created_at: now - 85_000, status: 'success', duration_ms: 190, error_code: null },
      ],
    },
    {
      upstream_account_id: observedAccount.id, upstream_name: observedAccount.name, model: 'gpt-5.4-mini',
      metrics: { requests: 2, successful_requests: 1, failed_requests: 1, avg_duration_ms: 405, p95_duration_ms: 660, costs: [] },
      health: { version: 'upstream_breaker_v1', status: 'degraded', observed_at: now - 5_000 },
      terminal_outcomes: [
        { id: 'req-between', source: 'request', created_at: now - 15_000, status: 'success', duration_ms: 405, error_code: null },
      ],
    },
  ],
};

function Fixture() {
  return <main className="main">
    <div className="hero compact"><div><span className="eyebrow">Fixture</span><h1>Upstream availability</h1></div></div>
    <article className="panel provider-list">
      <div className="account provider-account"><div className="account-main"><b>{observedAccount.name}</b><span>openai · API credential</span><UpstreamAvailability account={observedAccount} snapshot={snapshot} manualHealth={{ account_id: observedAccount.id, status: 'unhealthy', error_code: 'probe_timeout', upstream_status: 504, latency_ms: 1_200, checked_at: now - 1_000 }} onOpenRequest={(requestId) => { window.upstreamAvailabilityFixture.openedRequestId = requestId; }} /></div></div>
      <div className="account provider-account"><div className="account-main"><b>{unobservedAccount.name}</b><span>anthropic · API credential</span><UpstreamAvailability account={unobservedAccount} snapshot={snapshot} /></div></div>
    </article>
  </main>;
}

createRoot(document.getElementById('root')!).render(<I18nProvider><Fixture /></I18nProvider>);
