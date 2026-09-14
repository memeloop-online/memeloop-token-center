import { createRoot } from 'react-dom/client';
import { useEffect, useState } from 'react';

import { I18nProvider } from '../../src/i18n';
import { SessionReplayPanel } from '../../src/sessionReplayViews';
import type { ConversationRequest, LogicalSessionDetail, RequestDetail } from '../../src/types';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/sessionViews.css';

const sessionId = 'replay-session-01';
const longUserTurn = 'Find the forecast for Oslo, include precipitation probability and a concise packing recommendation for an early morning departure.'
  + ' Keep the evidence ordered by forecast hour and retain source caveats for the travel handoff.'.repeat(24);

function request(requestId: string, createdAt: number): ConversationRequest {
  return {
    request_id: requestId,
    created_at: createdAt,
    protocol: 'openai',
    model: 'fixture-session-replay-model',
    status_code: 200,
    duration_ms: 120,
    input_tokens: 24,
    output_tokens: 18,
    cost: '0',
    error_code: null,
    session_context: { session_id: sessionId, association: 'confirmed', session_name: null, task_kind: null, agent_id: null, semantics_source: 'declared' },
    source: 'live',
    provenance: 'native',
    unlinked: false,
  };
}

const requests = [request('replay-r1', 1), request('replay-r2', 2), request('replay-r3', 3), request('replay-r4', 4)];
const detail: LogicalSessionDetail = {
  session_id: sessionId,
  cluster_id: null,
  unlinked: false,
  requests,
  edges: [],
  has_more: true,
  next_cursor: { before_created_at: 0, before_request_id: 'older' },
  edges_truncated: false,
};

function archive(requestView: ConversationRequest, requestBody: unknown, responseBody: unknown, archiveComplete = true): RequestDetail {
  return { ...requestView, request_body: requestBody, response_body: responseBody, archive_complete: archiveComplete };
}

const archives = new Map<string, RequestDetail>([
  ['replay-r1', archive(requests[0]!,
    { input: [{ type: 'message', role: 'user', content: [{ type: 'input_text', text: longUserTurn }] }] },
    { output: [
      { type: 'message', role: 'assistant', content: [{ type: 'output_text', text: 'I will inspect the forecast source.' }] },
      { type: 'function_call', call_id: 'fixture-weather-call', name: 'weather_lookup', arguments: '{"city":"Oslo","units":"metric"}' },
    ] },
  )],
  ['replay-r2', archive(requests[1]!,
    { input: [{ type: 'function_call_output', call_id: 'fixture-weather-call', output: '{"precipitation_probability":70,"temperature_c":7}' }] },
    { output: [{ type: 'message', role: 'assistant', content: [{ type: 'output_text', text: 'Rain is likely. Pack a waterproof outer layer and allow extra travel time.' }] }] },
  )],
  ['replay-r3', archive(requests[2]!, null, null, false)],
  ['replay-r4', {
    ...archive(requests[3]!, { input: 'must not render for this session' }, { output: [] }),
    session_context: { session_id: 'different-session', association: 'confirmed', session_name: null, task_kind: null, agent_id: null, semantics_source: 'declared' },
  }],
]);

const liveFixture = new URLSearchParams(location.search).has('live');
declare global { interface Window { sessionReplayReads: Record<string, number>; sessionReplayAborts: number } }
window.sessionReplayReads = {};
window.sessionReplayAborts = 0;
let slowReleased = false;
let scopePaused = false;
const slowWaiters = new Set<() => void>();
const scopeWaiters = new Set<() => void>();
function waitForRelease(waiters: Set<() => void>, signal: AbortSignal) {
  return new Promise<void>((resolve, reject) => {
    const release = () => { waiters.delete(release); resolve(); };
    waiters.add(release);
    signal.addEventListener('abort', () => { waiters.delete(release); window.sessionReplayAborts += 1; reject(signal.reason); }, { once: true });
  });
}
function releaseAll(waiters: Set<() => void>) { for (const release of [...waiters]) release(); }
async function loadArchive(requestView: ConversationRequest, signal: AbortSignal) {
  window.sessionReplayReads[requestView.request_id] = (window.sessionReplayReads[requestView.request_id] ?? 0) + 1;
  if (liveFixture && scopePaused) await waitForRelease(scopeWaiters, signal);
  if (liveFixture && requestView.request_id === 'replay-r2' && !slowReleased) await waitForRelease(slowWaiters, signal);
  await new Promise<void>((resolve, reject) => {
    const timer = window.setTimeout(resolve, 12);
    signal.addEventListener('abort', () => { window.sessionReplayAborts += 1; window.clearTimeout(timer); reject(signal.reason); }, { once: true });
  });
  if (requestView.request_id === 'replay-r3' && requestView.archive_state === 'bound') return archive(requestView, null,
    { output: [{ type: 'message', role: 'assistant', content: [{ type: 'output_text', text: 'Late archive arrived' }] }] });
  if (requestView.archive_state === 'gap') throw new Error('fixture archive no longer available');
  const value = archives.get(requestView.request_id);
  if (requestView.request_id.startsWith('additional-')) return archive(requestView, null,
    { output: [{ type: 'message', role: 'assistant', content: [{ type: 'output_text', text: 'Additional completed archive' }] }] });
  if (!value) throw new Error('fixture archive unavailable');
  return value;
}

function Fixture() {
  const [current, setCurrent] = useState(detail);
  const [scope, setScope] = useState('scope-a');
  useEffect(() => {
    if (!liveFixture) return;
    const timer = window.setInterval(() => setCurrent(value => ({ ...value, requests: value.requests.map(request => ({ ...request })) })), 20);
    return () => window.clearInterval(timer);
  }, []);
  return <I18nProvider><main className="main" data-fixture-ready="session-replay">
    {liveFixture && <>
      <button onClick={() => setCurrent(value => ({ ...value, requests: [...value.requests, request(`additional-${value.requests.length}`, value.requests.length + 1)] }))}>Append live request</button>
      <button onClick={() => { scopePaused = true; setScope('scope-b'); }}>Switch replay scope</button>
      <button onClick={() => { scopePaused = false; releaseAll(scopeWaiters); }}>Resume scope reads</button>
      <button onClick={() => { slowReleased = true; releaseAll(slowWaiters); }}>Release slow archive</button>
      <button onClick={() => setCurrent(value => ({ ...value, requests: value.requests.map(request => request.request_id === 'replay-r3' ? { ...request, archive_state: 'bound' } : request) }))}>Finish late archive</button>
    </>}
    {liveFixture && <button onClick={() => setCurrent(value => ({ ...value, requests: value.requests.map(request => request.request_id === 'replay-r1' ? { ...request, archive_state: 'gap' } : request) }))}>Invalidate complete archive</button>}
    <SessionReplayPanel detail={current} scopeKey={scope} loadArchiveDetail={loadArchive} />
  </main></I18nProvider>;
}
createRoot(document.getElementById('root')!).render(<Fixture />);
