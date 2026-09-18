import { createRoot } from 'react-dom/client';
import { useEffect, useRef, useState } from 'react';
import { MtcFluentProvider } from '../../src/design-system';
import { I18nProvider } from '../../src/i18n';
import { RequestsPage } from '../../src/operator/pages/RequestsPage';
import type { RequestEvent, RequestListResponse, RequestView } from '../../src/types';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/styles/metrics.css';
import '../../src/styles/request-table.css';
import '../../src/styles/request-surfaces.css';
import '../../src/operator/operator.css';

declare global {
  interface Window {
    requestQueryReads: number;
    emitRequestOverflow: (count?: number) => Promise<void>;
    emitLiveRequest: (id: string, createdAt: number) => void;
    resolveNextRequestQuery: (id: string) => void;
    requestQueryKinds: string[];
  }
}

const request = (request_id: string, created_at: number): RequestView => ({
  request_id, created_at, completed_at: created_at + 1, protocol: 'openai', model: request_id,
  status_code: 200, duration_ms: 1, input_tokens: 1, output_tokens: 1, cost: '0',
  error_code: null, currency: 'USD', archive_state: 'bound', usage_basis: 'provider_reported',
});

const response = (id: string): RequestListResponse => {
  const requests = [request(id, 1_000), ...Array.from({ length: 99 }, (_, index) => request(`${id}-${index + 1}`, 999 - index))];
  const tail = requests.at(-1)!;
  return { requests, next_cursor: { before_created_at: tail.created_at, before_id: tail.request_id } };
};
const heldQueries: Array<{ active: boolean; resolve: (value: Response) => void }> = [];
window.requestQueryReads = 0;
window.requestQueryKinds = [];
window.emitRequestOverflow = async () => undefined;
window.emitLiveRequest = () => undefined;
window.resolveNextRequestQuery = id => {
  let held = heldQueries.shift();
  while (held && !held.active) held = heldQueries.shift();
  held?.resolve(Response.json(response(id)));
};

window.fetch = async (input, init) => {
  const url = new URL(String(input), location.origin);
  if (url.pathname === '/internal/v1/upstreams') return Response.json([]);
  if (url.pathname === '/internal/v1/requests/query') {
    window.requestQueryReads += 1;
    const body = JSON.parse(String(init?.body)) as { before_id?: string };
    const older = body.before_id !== undefined;
    window.requestQueryKinds.push(older ? 'older' : 'first');
    if (window.requestQueryReads === 1) return Response.json(response('initial-authoritative'));
    if (older) return Response.json({ requests: [request('older-page', 100)], next_cursor: null } satisfies RequestListResponse);
    return new Promise<Response>((resolve, reject) => {
      const held = { active: true, resolve };
      heldQueries.push(held);
      init?.signal?.addEventListener('abort', () => {
        held.active = false;
        reject(new DOMException('The operation was aborted', 'AbortError'));
      }, { once: true });
    });
  }
  return Response.json({ error: { message: `Unexpected fixture request: ${url.pathname}` } }, { status: 500 });
};

function Fixture() {
  const [overflowRevision, setOverflowRevision] = useState(0);
  const [revision, setRevision] = useState(0);
  const [events, setEvents] = useState(new Map<string, RequestEvent>());
  const resolveOverflowCommit = useRef<(() => void) | undefined>(undefined);
  window.emitRequestOverflow = (count = 1) => new Promise(resolve => {
    resolveOverflowCommit.current = resolve;
    setOverflowRevision(value => value + count);
  });
  useEffect(() => {
    const resolve = resolveOverflowCommit.current;
    resolveOverflowCommit.current = undefined;
    // Resolve after the complete passive-effect flush: RequestsPage has
    // observed the new revision and armed its coordinator before the test
    // advances the paused clock.
    if (resolve) queueMicrotask(resolve);
  }, [overflowRevision]);
  window.emitLiveRequest = (id, createdAt) => {
    const event: RequestEvent = {
      event_id: `event-${id}`, event_at: createdAt, event_kind: 'finished', request_id: id,
      key_id: 'fixture-key', created_at: createdAt, completed_at: createdAt + 1, protocol: 'openai', model: id,
      status_code: 200, duration_ms: 1, input_tokens: 1, output_tokens: 1, cost: '0', error_code: null,
      archive_state: 'bound', currency: 'USD', usage_basis: 'provider_reported',
    };
    setEvents(current => new Map(current).set(id, event));
    setRevision(value => value + 1);
  };
  return <>
    <output data-testid="overflow-revision" hidden>{overflowRevision}</output>
    <RequestsPage token="fixture-token" tenant="fixture-tenant" liveEvents={events}
      streamRevision={revision} streamOverflowRevision={overflowRevision} streamState="live" streamError=""
      onOpenSessions={() => undefined} onOpenSession={() => undefined} onProtectRequests={() => undefined}
      requestRefresh={{ intervalMs: 5_000, paused: false, onIntervalChange: () => undefined }} />
  </>;
}

createRoot(document.getElementById('root')!).render(<I18nProvider><MtcFluentProvider><Fixture /></MtcFluentProvider></I18nProvider>);
