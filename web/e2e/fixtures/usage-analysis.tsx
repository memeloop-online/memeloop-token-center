import { createRoot } from 'react-dom/client';

import { I18nProvider } from '../../src/i18n';
import { UsageAnalysis } from '../../src/operator/UsageAnalysis';
import type { OperatorUsageAnalysis } from '../../src/types';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/styles/metrics.css';
import '../../src/operator/operator.css';

declare global {
  interface Window { usageAnalysisFixture: { calls: string[] } }
}

const largeMetric = 1_250_000_000_000;
const metrics = {
  requests: largeMetric,
  success: largeMetric,
  failed: largeMetric,
  input_tokens: 312_500_000_000,
  output_tokens: 312_500_000_000,
  cached_input_tokens: largeMetric,
  cache_write_tokens: largeMetric,
  generation_units: largeMetric,
  avg_duration_ms: 1200,
  p95_duration_ms: 2500,
  costs: [],
};

const analysis: OperatorUsageAnalysis = {
  from_created_at: 1_700_000_000_000,
  to_created_at: 1_700_086_400_000,
  granularity: 'day',
  time_zone: 'UTC',
  p95_is_approximate: true,
  p95_method: 'fixture',
  upstream_grouping: 'stable_account',
  summary: metrics,
  time_series: [{ ...metrics, bucket_start: 1_700_000_000_000 }],
  by_model: [],
  by_key: [],
  by_session: [],
  by_upstream: [],
  by_protocol: [],
  by_status: [],
  errors: [],
  heatmap: [],
};

window.usageAnalysisFixture = { calls: [] };

function json(value: unknown, status = 200) {
  return new Response(JSON.stringify(value), {
    status,
    headers: { 'Content-Type': 'application/json' },
  });
}

globalThis.fetch = async (input: RequestInfo | URL) => {
  const url = new URL(typeof input === 'string' ? input : input.toString(), location.origin);
  window.usageAnalysisFixture.calls.push(`${url.pathname}${url.search}`);
  if (url.pathname === '/internal/v1/usage-analysis') return json(analysis);
  return json([]);
};

createRoot(document.getElementById('root')!).render(
  <I18nProvider><UsageAnalysis token="mts_usage_fixture" tenant="fixture-tenant" upstreams={[]} onOpenSession={() => undefined} /></I18nProvider>,
);
