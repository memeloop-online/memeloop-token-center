import { useState } from 'react';
import { createRoot } from 'react-dom/client';
import { MtcFluentProvider } from '../../src/design-system';
import { I18nProvider } from '../../src/i18n';
import { useRequestEventStream } from '../../src/operator/hooks/useRequestEventStream';
import type { RequestEvent } from '../../src/types';

declare global {
  interface Window {
    streamFetches: number;
    streamAborts: number;
    streamUrls: string[];
    emitStreamEvent: (eventId: string, eventAt: number) => void;
  }
}

window.streamFetches = 0;
window.streamAborts = 0;
window.streamUrls = [];
let activeController: ReadableStreamDefaultController<Uint8Array> | undefined;
const encoder = new TextEncoder();

window.fetch = async (input, init) => {
  window.streamFetches += 1;
  window.streamUrls.push(String(input));
  return new Response(new ReadableStream<Uint8Array>({
    start(controller) {
      activeController = controller;
      init?.signal?.addEventListener('abort', () => {
        window.streamAborts += 1;
        controller.error(new DOMException('The operation was aborted', 'AbortError'));
      }, { once: true });
    },
  }), { status: 200, headers: { 'content-type': 'text/event-stream' } });
};

window.emitStreamEvent = (eventId, eventAt) => {
  const event = {
    event_id: eventId,
    event_at: eventAt,
    event_kind: 'finished',
    request_id: `request-${eventId}`,
    key_id: 'fixture-key',
    status_code: 200,
    archive_state: 'pending',
    session_context: { association: 'confirmed', session_id: 'fixture-session' },
  } satisfies Partial<RequestEvent>;
  activeController?.enqueue(encoder.encode(`id: ${eventId}\nevent: request.finished\ndata: ${JSON.stringify(event)}\n\n`));
};

function Fixture() {
  const [events, setEvents] = useState<string[]>([]);
  const { state } = useRequestEventStream({
    token: 'fixture-token', tenant: 'fixture-tenant', enabled: true,
    disconnectedMessage: 'Fixture stream interrupted',
    onEvent: (event) => setEvents((current) => [...current, event.event_id]),
  });
  return <main><output data-testid="stream-state">{state}</output><output data-testid="stream-events">{events.join(',')}</output></main>;
}

createRoot(document.getElementById('root')!).render(<I18nProvider><MtcFluentProvider><Fixture /></MtcFluentProvider></I18nProvider>);
