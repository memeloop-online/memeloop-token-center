import { createRoot } from 'react-dom/client';
import { useState } from 'react';

import { RequestDiagnostics, RequestTable } from '../../src/components';
import { I18nProvider } from '../../src/i18n';
import type { RequestView } from '../../src/types';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/styles/request-table.css';

const request: RequestView = {
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

const historicalGap: RequestView = {
  request_id: '3d9f7abc-b767-4668-a8d1-baa042ea1df2',
  created_at: Date.UTC(2026, 8, 8, 12, 34, 56),
  completed_at: null,
  protocol: 'openai',
  model: 'fixture-long-model-name-for-request-observability',
  status_code: 429,
  duration_ms: 1234,
  input_tokens: 160,
  output_tokens: 32,
  error_code: 'http_429',
  upstream_account_id: null,
  route_id: null,
  currency: null,
  cost: '99',
  session_context: null,
};

function Fixture() {
  const [openedSession, setOpenedSession] = useState('');
  // The production shell places main in grid column two after its rail. Keep
  // the fixture's layout contract identical so narrow-table behavior is real.
  return <div className="app-shell" data-fixture-ready="request-diagnostics"><aside className="rail" aria-hidden="true" /><main className="main">
    <RequestTable requests={[request, historicalGap]} showRoutingDetails upstreamNames={new Map([[request.upstream_account_id!, 'Production Codex']])} currency="USD" onOpenSession={setOpenedSession} />
    <section data-fixture-request="recorded"><RequestDiagnostics request={request} currency="USD" upstreamName="Production Codex" onOpenSession={setOpenedSession} /></section>
    <section data-fixture-request="historical-gap"><RequestDiagnostics request={historicalGap} currency="USD" onOpenSession={setOpenedSession} /></section>
    <output data-fixture-session-opened="true">{openedSession}</output>
  </main></div>;
}

createRoot(document.getElementById('root')!).render(<I18nProvider><Fixture /></I18nProvider>);
