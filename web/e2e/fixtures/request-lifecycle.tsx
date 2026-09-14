import { createRoot } from 'react-dom/client';
import { useState } from 'react';
import { I18nProvider } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system';
import { RequestsPage } from '../../src/operator/pages/RequestsPage';
import type { RequestDetail, RequestEvent } from '../../src/types';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/styles/metrics.css';
import '../../src/styles/request-table.css';
import '../../src/operator/operator.css';

declare global { interface Window { requestLifecycleFixture: { finish: () => void; hold: () => void; release: () => void; switchScope: () => void; held: boolean; detailCalls: number }; } }
const base: RequestDetail = { request_id: 'request-a', created_at: 1000, completed_at: null, model: 'model-a', protocol: 'openai', status_code: null, duration_ms: null, input_tokens: 0, output_tokens: 0, cost: '0', error_code: null, request_body: null, response_body: null, archive_complete: false };
let first = base;
const second = { ...base, request_id: 'request-b', model: 'model-b', status_code: 200, completed_at: 3000, duration_ms: 2000 };
let hold = false;
let release: (() => void) | undefined;
const json = (value: unknown) => new Response(JSON.stringify(value), { headers: { 'Content-Type': 'application/json' } });
window.fetch = async (input) => {
  const url = new URL(String(input), location.origin);
  if (url.pathname === '/internal/v1/upstreams') return json([]);
  if (url.pathname === '/internal/v1/requests/query') return json({ requests: [first, second], next_cursor: null });
  if (url.pathname.startsWith('/internal/v1/requests/')) {
    window.requestLifecycleFixture.detailCalls += 1;
    const value = url.pathname.endsWith('/request-a') ? first : second;
    if (hold) {
      hold = false; window.requestLifecycleFixture.held = true;
      // Deliberately ignore abort to exercise the component's sequence fence.
      return new Promise<Response>((resolve) => { release = () => { window.requestLifecycleFixture.held = false; resolve(json(value)); }; });
    }
    return json(value);
  }
  return json([]);
};

function Fixture() {
  const [tenant, setTenant] = useState('tenant-a');
  const [revision, setRevision] = useState(0);
  const [events, setEvents] = useState<ReadonlyMap<string, RequestEvent>>(new Map());
  window.requestLifecycleFixture = {
    held: window.requestLifecycleFixture?.held ?? false, detailCalls: window.requestLifecycleFixture?.detailCalls ?? 0,
    hold: () => { hold = true; }, release: () => { release?.(); }, switchScope: () => setTenant('tenant-b'),
    finish: () => {
      first = { ...base, status_code: 200, completed_at: 3000, duration_ms: 2000, input_tokens: 20, output_tokens: 10, archive_complete: true };
      setEvents(new Map([[first.request_id, { ...first, event_id: `terminal-${revision}`, event_at: 3000, event_kind: 'finished', key_id: 'key' } as RequestEvent]]));
      setRevision((value) => value + 1);
    },
  };
  return <div data-request-fixture-scope={tenant}><RequestsPage token="fixture-token" tenant={tenant} liveEvents={events} streamRevision={revision} streamState="live" streamError="" onOpenSessions={() => {}} onOpenSession={() => {}} /></div>;
}
createRoot(document.getElementById('root')!).render(<I18nProvider><MtcFluentProvider><Fixture /></MtcFluentProvider></I18nProvider>);
