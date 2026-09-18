import { useRef, useState } from 'react';
import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { SessionMonitor } from '../../src/operator/SessionMonitor';
import { MtcFluentProvider } from '../../src/design-system';
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
    sessionListAborts: number;
    resolveSessionList: (status: boolean | number) => void;
    resolveSessionDetail: (status: boolean | number) => void;
  }
}
window.sessionListReads = 0;
window.sessionListAborts = 0;
window.sessionDetailReads = 0;
const controlledDetail = new URLSearchParams(location.search).has('controlled-detail');
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
    input_tokens: 2, output_tokens: 3, cost: '12.34', currency: 'USD', error_code: null,
    usage_basis: 'not_observed',
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
if (new URLSearchParams(location.search).has('titles')) {
  session.session_name = null;
  session.last_activity_at = 2_000;
  session.requests = 2;
  const base = sessionRequests[session.session_id]![0]!;
  sessionRequests[session.session_id] = [
    { ...base, request_id: 'newer-name', created_at: 2_000, session_context: { ...base.session_context!, session_name: 'Current context name' } },
    { ...base, request_id: 'older-name', created_at: 1_000, session_context: { ...base.session_context!, session_name: null }, execution: {
      session_name: 'Older execution name', trace_id: null, span_id: null, parent_span_id: null, agent_id: null, parent_agent_id: null, task_kind: null, labels: {}, source: 'declared',
    } },
  ];
}
window.fetch = async (input, init) => {
  const url = String(input);
  if (url.includes('/keys?')) return new Response(JSON.stringify([]));
  const summaryBatch = url.includes('/sessions/summaries');
  if (summaryBatch && init?.method !== 'POST') {
    return new Response(JSON.stringify({ error: { message: 'Fixture summary batches require POST' } }), { status: 405 });
  }
  if (summaryBatch || url.includes('/sessions?')) {
    window.sessionListReads += 1;
    return new Promise<Response>((resolve, reject) => {
      const signal = init?.signal;
      const abort = () => {
        window.sessionListAborts += 1;
        reject(new DOMException('The operation was aborted', 'AbortError'));
      };
      signal?.addEventListener('abort', abort, { once: true });
      window.resolveSessionList = (result) => {
        signal?.removeEventListener('abort', abort);
        const status = typeof result === 'number' ? result : result ? 200 : 503;
        const identities = summaryBatch
          ? (JSON.parse(String(init?.body ?? '{}')) as { identities?: Array<{ key_id: string; session_id: string }> }).identities ?? []
          : undefined;
        const summaries = identities
          ? identities.flatMap((identity) => [session, unlinkedSession]
            .filter((candidate) => candidate.key_id === identity.key_id && candidate.session_id === identity.session_id))
          : [session, unlinkedSession];
        resolve(new Response(JSON.stringify(status >= 200 && status < 300
          ? { generated_at: Date.now(), sessions: summaries, ...(identities ? {} : { next_cursor: null }) }
          : { error: { message: 'Fixture list unavailable' } }), { status }));
      };
    });
  }
  const detailSession = url.includes('/sessions/')
    ? decodeURIComponent(new URL(url, window.location.href).pathname.split('/').at(-1) ?? '')
    : session.session_id;
  if (url.includes('/sessions/')) window.sessionDetailReads += 1;
  const detailResponse = (result: boolean | number) => {
    const status = typeof result === 'number' ? result : result ? 200 : 503;
    return new Response(JSON.stringify(status >= 200 && status < 300 ? {
    session_id: detailSession, cluster_id: null, unlinked: detailSession.startsWith('unlinked:'),
    requests: sessionRequests[detailSession] ?? [], edges: [], has_more: false, next_cursor: null, edges_truncated: false,
    } : { error: { message: 'Fixture detail unavailable' } }), { status });
  };
  if (controlledDetail && url.includes('/sessions/')) {
    return new Promise<Response>((resolve, reject) => {
      const signal = init?.signal;
      const abort = () => reject(new DOMException('The operation was aborted', 'AbortError'));
      signal?.addEventListener('abort', abort, { once: true });
      window.resolveSessionDetail = (result) => {
        signal?.removeEventListener('abort', abort);
        resolve(detailResponse(result));
      };
    });
  }
  return detailResponse(true);
};
function Fixture() {
  const [revision, setRevision] = useState(0);
  const [refreshInterval, setRefreshInterval] = useState(0);
  const [backgroundPaused, setBackgroundPaused] = useState(false);
  const keys = useRef(new Set<string>());
  const overflowed = useRef(false);
  const queue = (keyId: string, sessionId: string, requestId = 'new-fixture-request',
    eventKind: RequestEventKind = 'finished', archiveState: RequestArchiveState = 'pending') => {
    enqueueSessionEventIdentity(keys.current, {
      key_id: keyId,
      request_id: requestId,
      event_kind: eventKind,
      status_code: 200,
      archive_state: archiveState,
      session_context: { association: 'confirmed', session_id: sessionId },
    });
  };
  const emit = (keyId: string, sessionId: string, requestId = 'new-fixture-request',
    eventKind: RequestEventKind = 'finished', archiveState: RequestArchiveState = 'pending') => {
    queue(keyId, sessionId, requestId, eventKind, archiveState);
    setRevision((value) => value + 1);
  };
  return <I18nProvider><MtcFluentProvider><main className="main">
    <button onClick={() => queue('fixture-key', 'fixture-session', 'queued-before-scope-change')}>Queue stale session event</button>
    <button onClick={() => emit('fixture-key', 'fixture-session')}>Simulate session event</button>
    <button onClick={() => emit('other-key', 'fixture-session')}>Simulate other credential event</button>
    <button onClick={() => emit('fixture-key', 'other-session')}>Simulate other session event</button>
    <button onClick={() => emit('fixture-key', 'fixture-session', 'archived-fixture-request', 'archive_bound', 'pending')}>Simulate archive bound event</button>
    <button onClick={() => emit('fixture-key', 'confirmed-session', 'projected-fixture-request', 'projected')}>Simulate confirmed projection</button>
    <button onClick={() => { overflowed.current = true; emit('other-key', 'other-session', 'overflow-retained-event'); }}>Simulate session event overflow</button>
    <button onClick={() => setBackgroundPaused(true)}>Pause shared refresh</button>
    <button onClick={() => setBackgroundPaused(false)}>Resume shared refresh</button>
    <SessionMonitor token="fixture-only" tenant="default" revision={revision} eventKeyIds={keys} eventOverflowed={overflowed} streamState="live"
      refreshCadence={{ intervalMs: refreshInterval, paused: backgroundPaused, onIntervalChange: setRefreshInterval }} onSelectRequest={async () => {}} />
  </main></MtcFluentProvider></I18nProvider>;
}
createRoot(document.getElementById('root')!).render(<Fixture />);
