import { createRoot } from 'react-dom/client';
import { useState } from 'react';
import { I18nProvider } from '../../src/i18n';
import { MonitoringSnapshot } from '../../src/operator/MonitoringSnapshot';
import { TypedFilterBuilder, emptyTypedFilterAst } from '../../src/operator/TypedFilterBuilder';
import type { OperatorMonitoringSnapshot } from '../../src/types';
import { requestOverviewFacts } from './request-overview-facts';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/styles/metrics.css';
import '../../src/operator/operator.css';

const health = { version: 'upstream_breaker_v1' as const, status: 'unknown' as const, observed_at: null };
const summary = { requests: 10, successful_requests: 9, failed_requests: 1, avg_duration_ms: 180, p95_duration_ms: 420, costs: [] };
const snapshot: OperatorMonitoringSnapshot = {
  contract_version: 'v1', generated_at: 0, scope: 'global',
  from_created_at: 0, to_created_at: 1000, granularity: 'hour',
  latency_is_approximate: true, latency_method: 'fixed_histogram_upper_bound_capped_60000ms', summary,
  freshness: { latest_terminal_created_at: null, age_millis: null }, health,
  top_upstream_models: requestOverviewFacts,
};

// The fixture has no service credential and never forwards application fetches.
globalThis.fetch = async () => new Response('{}', { status: 503 });
function Fixture() {
  const [ast, setAst] = useState(emptyTypedFilterAst);
  return <main style={{ padding: 8 }}>
    <TypedFilterBuilder ast={ast} onApply={setAst} onClear={() => setAst(emptyTypedFilterAst)} scope="requests" token="" tenant="" upstreams={[]} />
    <button type="button" id="outside-control">Outside control</button>
    <MonitoringSnapshot snapshot={snapshot} />
  </main>;
}
createRoot(document.getElementById('root')!).render(<I18nProvider><Fixture /></I18nProvider>);
