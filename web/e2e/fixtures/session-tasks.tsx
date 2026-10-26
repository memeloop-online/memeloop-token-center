import { useState } from 'react';
import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system';
import { SessionTitleAction } from '../../src/operator/SessionTitleAction';
import { SessionRelationships } from '../../src/operator/SessionRelationships';
import { SessionTaskPanel } from '../../src/operator/SessionTaskPanel';
import type { LogicalSessionSummary } from '../../src/types';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/sessionViews.css';
import '../../src/operator/operator.css';

declare global {
  interface Window {
    analyzeCalls: Array<{ scope: string; identities: number; timezone: string; include_subtrees: boolean }>;
    namePuts: Array<{ title: string; source: string; expected_revision: number }>;
    relationshipWrites: Array<{ method: string; kind: string }>;
    forceNameConflict: boolean;
    lastTitle: string;
    relatedActive: boolean;
  }
}
window.analyzeCalls = [];
window.namePuts = [];
window.relationshipWrites = [];
window.forceNameConflict = false;
window.lastTitle = '';
window.relatedActive = false;

const KEY = '00000000-0000-4000-8000-000000000001';
const SESSION = '00000000-0000-4000-8000-000000000002';
const PEER_KEY = '00000000-0000-4000-8000-000000000003';
const PEER_SESSION = '00000000-0000-4000-8000-000000000004';
const DAY = 86_400_000;
const FIRST = 1_792_800_000_000; // 2026-10-24T00:00:00Z

const alpha: LogicalSessionSummary = {
  session_id: SESSION, session_name: 'Parent task Alpha', task_kind: null,
  cluster_id: SESSION, unlinked: false, key_id: KEY, key_alias: 'Fixture key',
  model: 'fixture-model', protocol: 'openai', last_status: 'success', last_activity_at: FIRST + DAY,
  active_requests: 0, requests: 3, errors: 0, input_tokens: 100, output_tokens: 50,
  avg_duration_ms: 300, costs: [{ currency: 'USD', cost: '12.34' }], archived_only_requests: 0,
  archived_only_errors: 0, archived_only_input_tokens: 0, archived_only_output_tokens: 0,
  archived_only_avg_duration_ms: null,
};

const usage = (requests: number, input: number, output: number, micros: number) => ({
  requests, input_tokens: input, output_tokens: output, cached_input_tokens: 0,
  cache_write_tokens: 0, request_duration_sum_ms: 900, unknown_cost_requests: 0,
  costs: micros ? { USD: micros } : {},
});

function analysisFor(scope: string, identities: number) {
  return {
    scope,
    identities: Array.from({ length: identities }, () => ({ key_id: KEY, session_id: SESSION })),
    timezone: 'UTC',
    first_request_at: FIRST,
    last_request_at: FIRST + DAY + 3_600_000,
    calendar_span_days: 2,
    active_days: 2,
    totals: usage(3, 100, 50, 12_340_000),
    daily: [
      { day_bucket: FIRST / DAY, day_start_at: FIRST, usage: usage(1, 40, 20, 4_000_000) },
      { day_bucket: FIRST / DAY + 1, day_start_at: FIRST + DAY, usage: usage(2, 60, 30, 8_340_000) },
    ],
    coverage: 'completed_request_facts_and_archive_only',
  };
}

function json(body: unknown, status = 200) {
  return new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });
}

window.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
  const url = String(input);
  const method = init?.method ?? 'GET';
  if (url.includes('/sessions/parents')) return json({ accepted: 0, linked: 0 });
  if (url.includes('/sessions/resolve')) return json({ sessions: [] });
  if (url.includes('/sessions/analyze')) {
    const body = JSON.parse(String(init?.body ?? '{}'));
    window.analyzeCalls.push({ scope: body.scope, identities: body.identities.length, timezone: body.timezone, include_subtrees: body.include_subtrees });
    return json(analysisFor(body.scope, body.identities.length));
  }
  if (url.includes(`/sessions/${SESSION}/name`)) {
    const body = JSON.parse(String(init?.body ?? '{}'));
    window.namePuts.push(body);
    if (window.forceNameConflict) return json({ error: { message: 'name revision changed or manual name is protected' } }, 409);
    window.lastTitle = body.title;
    return json({ title: body.title, state: 'ready', error_code: null, updated_at: FIRST, source: 'manual', revision: window.namePuts.length });
  }
  if (url.includes(`/sessions/${SESSION}/title`)) {
    if (method === 'POST') return json({ title: null, state: 'pending', error_code: null, source: 'generated', revision: 0 });
    return json(window.lastTitle
      ? { title: window.lastTitle, state: 'ready', error_code: null, updated_at: FIRST, source: 'manual', revision: window.namePuts.length }
      : { title: null, state: 'unavailable', error_code: null, source: 'generated', revision: 0 });
  }
  if (url.includes(`/sessions/${SESSION}/relationships`)) {
    if (method === 'GET') {
      return json({
        relationships: window.relatedActive
          ? [{ key_id: KEY, session_id: SESSION, target: { key_id: PEER_KEY, session_id: PEER_SESSION }, kind: 'related', evidence: null }]
          : [],
      });
    }
    const body = JSON.parse(String(init?.body ?? '{}'));
    window.relationshipWrites.push({ method, kind: body.kind });
    if (method === 'PUT' && body.target?.session_id === PEER_SESSION) window.relatedActive = true;
    if (method === 'DELETE') window.relatedActive = false;
    return json({ ok: true });
  }
  if (url.includes('/sessions/summaries')) {
    return json({
      generated_at: FIRST,
      sessions: [{ ...alpha, session_id: PEER_SESSION, key_id: PEER_KEY, session_name: 'Related Beta' }],
    });
  }
  if (url.includes('/internal/v1/sessions?')) {
    return json({ generated_at: FIRST, sessions: url.includes('q=Alpha') ? [alpha] : [], next_cursor: null });
  }
  return json({ error: { message: `unhandled fixture route: ${method} ${url}` } }, 500);
};

function Fixture() {
  const [title, setTitle] = useState('');
  return <MtcFluentProvider><I18nProvider>
    <p data-testid="title-echo">{title}</p>
    <SessionTitleAction token="fixture-token" tenant="default" keyId={KEY} sessionId={SESSION} onTitle={setTitle} />
    <SessionRelationships token="fixture-token" tenant="default" identity={{ key_id: KEY, session_id: SESSION }} />
    <SessionTaskPanel token="fixture-token" tenant="default" />
  </I18nProvider></MtcFluentProvider>;
}

createRoot(document.getElementById('root')!).render(<Fixture />);
