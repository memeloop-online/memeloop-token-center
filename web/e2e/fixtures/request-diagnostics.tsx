import { createRoot } from 'react-dom/client';
import { useState } from 'react';

import { NumberMetric, RequestDiagnostics, RequestTable } from '../../src/components';
import { I18nProvider } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system';
import type { RequestView } from '../../src/types';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/styles/request-table.css';

const request: RequestView = {
  usage_basis: 'provider_reported',
  request_id: '5a3bc0cc-8d47-4cee-9b5e-2581f8d99d13',
  created_at: Date.UTC(2026, 8, 8, 12, 34, 56),
  completed_at: Date.UTC(2026, 8, 8, 12, 34, 57, 234),
  protocol: 'openai',
  model: 'fixture-long-model-name-for-request-observability',
  status_code: 429,
  duration_ms: 1234,
  input_tokens: 160,
  cached_input_tokens: 40,
  cache_write_tokens: 20,
  output_tokens: 32,
  cost: '0.001234',
  credential_identity: { tenant_external_id: 'fixture', key_id: 'fixture-key-id', key_alias: 'Research key', principal_external_id: 'research-team' },
  upstream_account_id: 'e82ea007-9b7f-4be9-bf18-6829426a94e5',
  route_id: 'a75fc2f6-e145-4596-bc94-9736271c6d7e',
  currency: 'USD',
  error_code: 'http_429',
  session_context: {
    session_id: '726d4c8a-6641-4cb3-98bf-b21a64e4208f',
    association: 'confirmed',
    session_name: 'Fixture session',
    task_kind: 'turn',
    agent_id: 'fixture-agent',
    semantics_source: 'declared',
  },
};

if (new URLSearchParams(location.search).get('usage-basis') === 'contract_ceiling') {
  Object.assign(request, { usage_basis: 'contract_ceiling', status_code: 499, output_tokens: 100000, duration_ms: 11681, first_output_ms: 1000, generation_duration_ms: 10681 });
}
if (new URLSearchParams(location.search).get('usage-basis') === 'not_observed') {
  Object.assign(request, { usage_basis: 'not_observed', status_code: 502, error_code: 'upstream_stream', cost: '35' });
}

const historicalGap: RequestView = {
  request_id: '3d9f7abc-b767-4668-a8d1-baa042ea1df2',
  created_at: Date.UTC(2026, 8, 8, 12, 34, 56),
  completed_at: null,
  protocol: 'openai',
  model: 'fixture-long-model-name-for-request-observability',
  status_code: 429,
  duration_ms: null,
  credential_identity: null,
  input_tokens: 160,
  output_tokens: 32,
  error_code: 'http_429',
  upstream_account_id: null,
  route_id: null,
  currency: null,
  cost: '99',
  session_context: null,
};

// Exercise unbroken provider/session metadata without altering ordinary fixtures.
if (new URLSearchParams(location.search).has('long-tokens') && request.session_context) {
  request.session_context.session_name = 'session_'.repeat(40);
  request.session_context.agent_id = 'agent_'.repeat(40);
}

function Fixture() {
  const [openedSession, setOpenedSession] = useState('');
  // The production shell places main in grid column two after its rail. Keep
  // the fixture's layout contract identical so narrow-table behavior is real.
  return <div className="app-shell" data-fixture-ready="request-diagnostics"><aside className="rail" aria-hidden="true" /><main className="main">
    <section className="metrics request-traffic-metrics">{['Requests', 'Success', 'Failure', 'Running', 'Success rate', 'Average latency'].map(label => <NumberMetric key={label} label={label} value={100} />)}</section>
    <RequestTable requests={[request, historicalGap, { ...request, request_id: 'running-request', status_code: null, duration_ms: null, completed_at: null, input_tokens: 0, cached_input_tokens: 0, cache_write_tokens: 0, output_tokens: 0, cost: '0', error_code: null }, { ...request, request_id: 'timed-request', status_code: 200, error_code: null, first_output_ms: 234, generation_duration_ms: 1000 }]} upstreamNames={new Map([[request.upstream_account_id!, 'Production Codex · csil.ai.automation@example.test']])} currency="USD" onOpenSession={setOpenedSession} />
    <section data-fixture-request="recorded"><RequestDiagnostics request={request} currency="USD" upstreamName="Production Codex" onOpenSession={setOpenedSession} /></section>
    <section data-fixture-request="historical-gap"><RequestDiagnostics request={historicalGap} currency="USD" onOpenSession={setOpenedSession} /></section>
    <output data-fixture-session-opened="true">{openedSession}</output>
  </main></div>;
}

createRoot(document.getElementById('root')!).render(<I18nProvider><MtcFluentProvider><Fixture /></MtcFluentProvider></I18nProvider>);
