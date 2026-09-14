import { createRoot } from 'react-dom/client';
import { useLayoutEffect, useState } from 'react';
import { Tooltip } from '@fluentui/react-components';
import { I18nProvider } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system';
import { RequestsPage } from '../../src/operator/pages/RequestsPage';
import type { RequestDetail, RequestEvent, TypedFilterAst } from '../../src/types';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/styles/metrics.css';
import '../../src/styles/request-table.css';
import '../../src/operator/operator.css';

declare global { interface Window { requestLifecycleFixture: { finish: () => void; hold: () => void; release: () => void; switchScope: () => void; filter: () => void; failQuery: () => void; held: boolean; queryHeld: boolean; detailCalls: number; scopeCommits: { scope: string; drawers: number }[] }; } }
const base: RequestDetail = { request_id: 'request-a', created_at: 1000, completed_at: null, model: 'model-a', protocol: 'openai', status_code: null, duration_ms: null, input_tokens: 0, output_tokens: 0, cost: '0', error_code: null, request_body: null, response_body: null, archive_complete: false };
let first = base;
const second = { ...base, request_id: 'request-b', model: 'model-b', status_code: 200, completed_at: 3000, duration_ms: 2000 };
let hold = false;
let release: (() => void) | undefined;
let holdQuery = false;
let failQuery: (() => void) | undefined;
const json = (value: unknown) => new Response(JSON.stringify(value), { headers: { 'Content-Type': 'application/json' } });
window.fetch = async (input) => {
  const url = new URL(String(input), location.origin);
  if (url.pathname === '/internal/v1/upstreams') return json([]);
  if (url.pathname === '/internal/v1/requests/query') {
    if (holdQuery) {
      holdQuery = false; window.requestLifecycleFixture.queryHeld = true;
      return new Promise<Response>((resolve) => { failQuery = () => { window.requestLifecycleFixture.queryHeld = false; resolve(new Response(JSON.stringify({ error: 'fixture query failure' }), { status: 503 })); }; });
    }
    return json({ requests: [first, second], next_cursor: { before_created_at: 1000, before_id: second.request_id } });
  }
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
  const [drilldown, setDrilldown] = useState<{ ast: TypedFilterAst; revision: number }>();
  window.requestLifecycleFixture = {
    held: window.requestLifecycleFixture?.held ?? false, detailCalls: window.requestLifecycleFixture?.detailCalls ?? 0,
    scopeCommits: window.requestLifecycleFixture?.scopeCommits ?? [],
    queryHeld: window.requestLifecycleFixture?.queryHeld ?? false,
    filter: () => { holdQuery = true; setDrilldown({ ast: { logical_operator: 'and', conditions: [{ field: 'model', operator: 'equals', value: { type: 'model', value: 'different-model' } }] }, revision: 1 }); },
    failQuery: () => { failQuery?.(); },
    hold: () => { hold = true; }, release: () => { release?.(); }, switchScope: () => setTenant('tenant-b'),
    finish: () => {
      first = { ...base, status_code: 200, completed_at: 3000, duration_ms: 2000, input_tokens: 20, output_tokens: 10, archive_complete: true };
      setEvents(new Map([[first.request_id, { ...first, event_id: `terminal-${revision}`, event_at: 3000, event_kind: 'finished', key_id: 'key' } as RequestEvent]]));
      setRevision((value) => value + 1);
    },
  };
  useLayoutEffect(() => {
    window.requestLifecycleFixture.scopeCommits.push({ scope: tenant, drawers: document.querySelectorAll('.drawer').length });
  }, [tenant]);
  return <div data-request-fixture-scope={tenant}><RequestsPage token="fixture-token" tenant={tenant} liveEvents={events} streamRevision={revision} streamState="live" streamError="" onOpenSessions={() => {}} onOpenSession={() => {}} requestDrilldown={drilldown} /><Tooltip content="Background scope help" visible relationship="description"><button id="background-help">Background helper</button></Tooltip></div>;
}
createRoot(document.getElementById('root')!).render(<I18nProvider><MtcFluentProvider><Fixture /></MtcFluentProvider></I18nProvider>);
