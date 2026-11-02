import { createRoot } from 'react-dom/client';
import { useState } from 'react';
import { MtcFluentProvider } from '../../src/design-system';
import { I18nProvider, useI18n } from '../../src/i18n';
import { useOperatorRequestStream } from '../../src/operator/hooks/useOperatorRequestStream';
import { RequestsPage } from '../../src/operator/pages/RequestsPage';
import type { RequestDetail, RequestEvent, RequestSessionContext, RequestView } from '../../src/types';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/styles/metrics.css';
import '../../src/styles/request-table.css';
import '../../src/operator/operator.css';

declare global {
  interface Window {
    requestNavigationFixture: {
      queryCalls: number;
      detailCalls: number;
      upstreamCalls: number;
      streamCalls: number;
      queryHeld: boolean;
      detailHeld: boolean;
      streamOpen: boolean;
      emit: (kind: 'started' | 'finished') => void;
      releaseQuery: () => void;
      releaseDetail: () => void;
    };
  }
}

const unlinkedContext: RequestSessionContext = {
  session_id: null, association: 'unlinked', session_name: null,
  task_kind: null, agent_id: null, semantics_source: null,
};
const confirmedContext: RequestSessionContext = {
  session_id: 'session-alpha', association: 'confirmed', session_name: '深夜调研会话',
  task_kind: 'chat', agent_id: 'agent-7', semantics_source: 'declared',
};
const restRow: RequestView = {
  request_id: 'request-live', created_at: 2_000, completed_at: 2_800, protocol: 'openai',
  model: 'model-live', status_code: 200, duration_ms: 800, input_tokens: 12, output_tokens: 4,
  cost: '0.002', error_code: null, currency: 'USD', archive_state: 'bound',
  usage_basis: 'provider_reported', session_context: confirmedContext,
  egress: { selected: { kind: 'direct', group_id: null, group_version: null, group_name: null,
    member_id: null, member_name: null, scheme: null, host: null, port: null }, observed_at: 2_000 },
};
const detail: RequestDetail = { ...restRow, session_context: unlinkedContext, egress: null,
  request_body: null, response_body: null, archive_complete: true };

function streamEvent(kind: 'started' | 'finished'): RequestEvent {
  const finished = kind === 'finished';
  return {
    event_id: finished ? 'nav-event-finished' : 'nav-event-started',
    event_at: finished ? 2_800 : 2_000,
    event_kind: kind,
    request_id: restRow.request_id,
    created_at: 2_000,
    completed_at: finished ? 2_800 : null,
    key_id: 'fixture-key',
    protocol: 'openai',
    model: restRow.model,
    status_code: finished ? 200 : null,
    duration_ms: finished ? 800 : null,
    input_tokens: 12,
    output_tokens: finished ? 4 : 0,
    cost: finished ? '0.002' : null,
    error_code: null,
    archive_state: finished ? 'bound' : 'pending',
    session_context: { ...unlinkedContext },
  };
}

const encoder = new TextEncoder();
let streamController: ReadableStreamDefaultController<Uint8Array> | undefined;
const heldQueries: Array<{ active: boolean; resolve: (response: Response) => void }> = [];
let detailResolve: (() => void) | undefined;

window.requestNavigationFixture = {
  queryCalls: 0,
  detailCalls: 0,
  upstreamCalls: 0,
  streamCalls: 0,
  queryHeld: false,
  detailHeld: false,
  streamOpen: false,
  emit(kind) {
    if (!streamController) throw new Error('fixture stream is not connected');
    const event = streamEvent(kind);
    streamController.enqueue(encoder.encode(`id: ${event.event_id}\nevent: request.${event.event_kind}\ndata: ${JSON.stringify(event)}\n\n`));
  },
  releaseQuery() {
    const held = heldQueries.find((entry) => entry.active);
    if (!held) throw new Error('no held query to release');
    held.active = false;
    window.requestNavigationFixture.queryHeld = heldQueries.some((entry) => entry.active);
    held.resolve(Response.json({ requests: [restRow], next_cursor: null }));
  },
  releaseDetail() {
    if (!detailResolve) throw new Error('no held detail to release');
    const resolve = detailResolve;
    detailResolve = undefined;
    window.requestNavigationFixture.detailHeld = false;
    resolve();
  },
};

window.fetch = async (input, init) => {
  const url = new URL(String(input), location.origin);
  if (url.pathname === '/internal/v1/request-events') {
    window.requestNavigationFixture.streamCalls += 1;
    return new Response(new ReadableStream<Uint8Array>({
      start(controller) {
        streamController = controller;
        window.requestNavigationFixture.streamOpen = true;
        init?.signal?.addEventListener('abort', () => {
          window.requestNavigationFixture.streamOpen = false;
          controller.error(new DOMException('The operation was aborted', 'AbortError'));
        }, { once: true });
      },
    }), { status: 200, headers: { 'content-type': 'text/event-stream' } });
  }
  if (url.pathname === '/internal/v1/upstreams') {
    window.requestNavigationFixture.upstreamCalls += 1;
    return Response.json([]);
  }
  if (url.pathname === '/internal/v1/requests/query') {
    window.requestNavigationFixture.queryCalls += 1;
    window.requestNavigationFixture.queryHeld = true;
    return new Promise<Response>((resolve, reject) => {
      const held = { active: true, resolve };
      heldQueries.push(held);
      init?.signal?.addEventListener('abort', () => {
        held.active = false;
        window.requestNavigationFixture.queryHeld = heldQueries.some((entry) => entry.active);
        reject(new DOMException('The operation was aborted', 'AbortError'));
      }, { once: true });
    });
  }
  if (url.pathname.startsWith('/internal/v1/requests/')) {
    window.requestNavigationFixture.detailCalls += 1;
    window.requestNavigationFixture.detailHeld = true;
    return new Promise<Response>((resolve, reject) => {
      detailResolve = () => resolve(Response.json(detail));
      init?.signal?.addEventListener('abort', () => {
        if (detailResolve) {
          detailResolve = undefined;
          window.requestNavigationFixture.detailHeld = false;
        }
        reject(new DOMException('The operation was aborted', 'AbortError'));
      }, { once: true });
    });
  }
  return Response.json({ error: { message: `Unexpected fixture request: ${url.pathname}` } }, { status: 500 });
};

function Fixture() {
  const { t } = useI18n();
  const [tab, setTab] = useState<'requests' | 'sessions' | 'overview'>('requests');
  const stream = useOperatorRequestStream({
    token: 'fixture-token',
    tenant: 'fixture-tenant',
    enabled: tab !== 'overview',
    disconnectedMessage: t('traffic.streamDisconnected'),
    intervalMs: 0,
  });
  return <main data-stream-revision={stream.revision}>
    <div role="group" aria-label="fixture navigation">
      <button type="button" data-testid="tab-requests" aria-current={tab === 'requests' ? 'page' : undefined} onClick={() => setTab('requests')}>请求</button>
      <button type="button" data-testid="tab-other" aria-current={tab === 'sessions' ? 'page' : undefined} onClick={() => setTab('sessions')}>会话</button>
      <button type="button" data-testid="tab-overview" aria-current={tab === 'overview' ? 'page' : undefined} onClick={() => setTab('overview')}>概览</button>
    </div>
    {tab === 'requests'
      ? <RequestsPage
          token="fixture-token"
          tenant="fixture-tenant"
          liveEvents={stream.events.current}
          streamRevision={stream.revision}
          streamState={stream.state}
          streamError={stream.error}
          streamOverflowRevision={stream.overflowRevision}
          onProtectRequests={stream.protectRequests}
          onOpenSessions={() => undefined}
          onOpenSession={() => undefined} />
      : <section data-testid="other-panel"><h2>{tab === 'sessions' ? '会话' : '概览'}</h2></section>}
  </main>;
}

createRoot(document.getElementById('root')!).render(<I18nProvider><MtcFluentProvider><Fixture /></MtcFluentProvider></I18nProvider>);
