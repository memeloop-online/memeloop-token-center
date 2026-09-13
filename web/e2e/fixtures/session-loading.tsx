import { useRef, useState } from 'react';
import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { SessionMonitor } from '../../src/operator/SessionMonitor';
import { enqueueSessionEventIdentity } from '../../src/operator/sessionRefresh';
import type { LogicalSessionDetail, LogicalSessionSummary, RequestArchiveState, RequestEventKind } from '../../src/types';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/sessionViews.css';
import '../../src/operator/operator.css';

declare global {
  interface Window {
    sessionDetailReads: number;
    sessionListReads: number;
    resolveSessionList: (ok: boolean) => void;
  }
}
window.sessionListReads = 0;
window.sessionDetailReads = 0;
const session: LogicalSessionSummary = {
  session_id: 'fixture-session', session_name: 'Retained session', task_kind: null,
  cluster_id: null, unlinked: false, key_id: 'fixture-key', key_alias: 'Fixture key',
  model: 'fixture-model', protocol: 'openai', last_status: 'success', last_activity_at: 1000,
  active_requests: 0, requests: 1, errors: 0, input_tokens: 2, output_tokens: 3,
  avg_duration_ms: 20, costs: [], archived_only_requests: 0, archived_only_errors: 0,
  archived_only_input_tokens: 0, archived_only_output_tokens: 0, archived_only_avg_duration_ms: null,
};
const unlinkedSession: LogicalSessionSummary = {
  ...session,
  session_id: 'unlinked:fixture-key', session_name: null, unlinked: true,
  last_activity_at: 900,
};
const sessionRequests: Record<string, LogicalSessionDetail['requests']> = {
  [session.session_id]: [{
    request_id: 'archived-fixture-request', created_at: 1000, completed_at: 1010,
    protocol: 'openai', model: 'fixture-model', status_code: 200, duration_ms: 10,
    input_tokens: 2, output_tokens: 3, cost: '0', currency: 'USD', error_code: null,
    archive_state: 'pending', source: 'live', provenance: 'native', unlinked: false,
    session_context: {
      session_id: session.session_id, association: 'confirmed', session_name: session.session_name,
      task_kind: null, agent_id: null, semantics_source: 'declared',
    },
  }],
  [unlinkedSession.session_id]: [{
    request_id: 'projected-fixture-request', created_at: 900, completed_at: 910,
    protocol: 'openai', model: 'fixture-model', status_code: 200, duration_ms: 10,
    input_tokens: 2, output_tokens: 3, cost: '0', currency: 'USD', error_code: null,
    archive_state: 'pending', source: 'live', provenance: 'native', unlinked: true,
    session_context: {
      session_id: null, association: 'unlinked', session_name: null,
      task_kind: null, agent_id: null, semantics_source: null,
    },
  }],
};
window.fetch = async (input) => {
  const url = String(input);
  if (url.includes('/sessions?')) {
    window.sessionListReads += 1;
    return new Promise<Response>((resolve) => {
      window.resolveSessionList = (ok) => resolve(new Response(JSON.stringify(ok
        ? { generated_at: Date.now(), sessions: [session, unlinkedSession], next_cursor: null }
        : { error: { message: 'Fixture list unavailable' } }), { status: ok ? 200 : 503 }));
    });
  }
  const detailSession = url.includes('/sessions/')
    ? decodeURIComponent(new URL(url, window.location.href).pathname.split('/').at(-1) ?? '')
    : session.session_id;
  if (url.includes('/sessions/')) window.sessionDetailReads += 1;
  return new Response(JSON.stringify({
    session_id: detailSession, cluster_id: null, unlinked: detailSession.startsWith('unlinked:'),
    requests: sessionRequests[detailSession] ?? [], edges: [], has_more: false, next_cursor: null, edges_truncated: false,
  }));
};
function Fixture() {
  const [revision, setRevision] = useState(0);
  const keys = useRef(new Set<string>());
  const emit = (keyId: string, sessionId: string, requestId = 'new-fixture-request',
    eventKind: RequestEventKind = 'finished', archiveState: RequestArchiveState = 'pending') => {
    enqueueSessionEventIdentity(keys.current, {
      key_id: keyId,
      request_id: requestId,
      event_kind: eventKind,
      status_code: 200,
      archive_state: archiveState,
      session_context: { association: 'confirmed', session_id: sessionId },
    });
    setRevision((value) => value + 1);
  };
  return <I18nProvider><main className="main">
    <button onClick={() => emit('fixture-key', 'fixture-session')}>Simulate session event</button>
    <button onClick={() => emit('other-key', 'fixture-session')}>Simulate other credential event</button>
    <button onClick={() => emit('fixture-key', 'other-session')}>Simulate other session event</button>
    <button onClick={() => emit('fixture-key', 'fixture-session', 'archived-fixture-request', 'archive_bound', 'bound')}>Simulate archive bound event</button>
    <button onClick={() => emit('fixture-key', 'confirmed-session', 'projected-fixture-request', 'projected')}>Simulate confirmed projection</button>
    <SessionMonitor token="fixture-only" tenant="default" revision={revision} eventKeyIds={keys} streamState="live" onSelectRequest={async () => {}} />
  </main></I18nProvider>;
}
createRoot(document.getElementById('root')!).render(<Fixture />);
