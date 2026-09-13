import { useRef, useState } from 'react';
import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { SessionMonitor } from '../../src/operator/SessionMonitor';
import { enqueueSessionEventIdentity } from '../../src/operator/sessionRefresh';
import type { LogicalSessionSummary } from '../../src/types';
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
window.fetch = async (input) => {
  if (String(input).includes('/sessions?')) {
    window.sessionListReads += 1;
    return new Promise<Response>((resolve) => {
      window.resolveSessionList = (ok) => resolve(new Response(JSON.stringify(ok
        ? { generated_at: Date.now(), sessions: [session], next_cursor: null }
        : { error: { message: 'Fixture list unavailable' } }), { status: ok ? 200 : 503 }));
    });
  }
  if (String(input).includes('/sessions/')) window.sessionDetailReads += 1;
  return new Response(JSON.stringify({ session_id: session.session_id, cluster_id: null, unlinked: false, requests: [], edges: [], has_more: false, next_cursor: null, edges_truncated: false }));
};
function Fixture() {
  const [revision, setRevision] = useState(0);
  const keys = useRef(new Set<string>());
  const emit = (keyId: string, sessionId: string) => {
    enqueueSessionEventIdentity(keys.current, {
      key_id: keyId,
      request_id: 'new-fixture-request',
      event_kind: 'finished',
      status_code: 200,
      session_context: { association: 'confirmed', session_id: sessionId },
    });
    setRevision((value) => value + 1);
  };
  return <I18nProvider><main className="main">
    <button onClick={() => emit('fixture-key', 'fixture-session')}>Simulate session event</button>
    <button onClick={() => emit('other-key', 'fixture-session')}>Simulate other credential event</button>
    <button onClick={() => emit('fixture-key', 'other-session')}>Simulate other session event</button>
    <SessionMonitor token="fixture-only" tenant="default" revision={revision} eventKeyIds={keys} streamState="live" onSelectRequest={async () => {}} />
  </main></I18nProvider>;
}
createRoot(document.getElementById('root')!).render(<Fixture />);
