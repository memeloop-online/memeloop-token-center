import { createRoot } from 'react-dom/client';
import { useLayoutEffect, useState } from 'react';
import { Tooltip } from '@fluentui/react-components';
import { I18nProvider } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system';
import { RequestsPage } from '../../src/operator/pages/RequestsPage';
import { coalesceRequestEvent, RequestRefreshBatch } from '../../src/operator/traffic/requestRefresh';
import type { RequestDetail, RequestEvent, TypedFilterAst } from '../../src/types';
import { egressAttempts } from '../support/request-egress-fixtures';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/styles/metrics.css';
import '../../src/styles/request-table.css';
import '../../src/operator/operator.css';

declare global { interface Window { requestLifecycleFixture: { finish: () => void; settle: (cost: string, kind: 'finished' | 'projected' | 'archive_bound') => void; replay: () => void; hold: () => void; release: () => void; switchScope: () => void; filter: () => void; failQuery: () => void; held: boolean; queryHeld: boolean; queryCalls: number; detailCalls: number; scopeCommits: { scope: string; drawers: number }[] }; } }
const base: RequestDetail = { request_id: 'request-a', created_at: 1000, completed_at: null, model: 'model-a', protocol: 'openai', status_code: null, duration_ms: null, input_tokens: 0, output_tokens: 0, cost: '0', error_code: null, request_body: null, response_body: null, archive_complete: false };
const backgroundHelpVisible = !new URLSearchParams(location.search).has('quiet-background-help');
const supplierFailure = new URLSearchParams(location.search).has('supplier-error')
  ? { ...base, status_code: 402, completed_at: 3000, duration_ms: 2000, error_code: 'http_402',
    supplier_error: { code: 'no_active_plan', message: '当前账号没有可用套餐; provider code: 402; provider message: 当前账号没有可用套餐' } } : undefined;
const diagnosticParam = new URLSearchParams(location.search).get('failure-diagnostic');
const diagnosticFailure = diagnosticParam === 'downstream'
  ? { ...base, status_code: 499, completed_at: 3000, duration_ms: 2000, error_code: 'http_499',
    failure_diagnostic: { stage: 'response_delivery', source: 'downstream_connection', reason: 'client_disconnected' } }
  : diagnosticParam === 'supplier'
  ? { ...base, status_code: 499, completed_at: 3000, duration_ms: 2000, error_code: 'http_499',
    failure_diagnostic: { stage: 'upstream_response_headers', source: 'upstream_http_response', reason: 'upstream_http_status' } }
  : undefined;
const operationalMode = new URLSearchParams(location.search).get('operational');
const operational: RequestDetail | undefined = operationalMode ? {
  ...base, currency: 'USD', status_code: operationalMode === '499' ? 499 : operationalMode === '502' ? 502 : 200,
  completed_at: operationalMode === 'zero' ? 4000 : null, duration_ms: 2000,
  cost: operationalMode === 'zero' ? '0' : null,
  error_code: operationalMode === '499' ? 'http_499' : operationalMode === '502' ? 'http_502' : null,
  terminal_cause_code: operationalMode === '499' ? 'client_disconnected' : operationalMode === '502' ? 'upstream_transport_connection_reset' : null,
  operational_terminal: { completed_at: 3000, stage: null, financial_state: operationalMode === 'zero' ? 'applied' : operationalMode === '200' ? 'unknown' : 'pending' },
} : undefined;
let first = operational ?? supplierFailure ?? diagnosticFailure ?? base;
if (new URLSearchParams(location.search).has('egress')) first = {
  ...base, status_code: 502, completed_at: 3000, duration_ms: 2000, error_code: 'upstream_http2_reset',
  egress_attempts: egressAttempts,
};
const second = { ...base, compaction: null, request_id: 'request-b', model: 'model-b', status_code: 200, completed_at: 3000, duration_ms: 2000 };
// Synthetic fixtures only: documentation ranges (RFC 5737), no real user data.
const egressLineRows: RequestDetail[] = [
  { ...base, request_id: 'request-c', model: 'model-c', status_code: 200, completed_at: 3000, duration_ms: 2000,
    egress: { selected: { kind: 'proxy', group_id: 'group-c', group_version: 3, group_name: '示例出口组', member_id: 'member-c', member_name: 'edge-01', scheme: 'socks5h', host: '192.0.2.10', port: 1080 }, observed_at: 1760000000000 } },
  { ...base, request_id: 'request-d', model: 'model-d', status_code: 200, completed_at: 3000, duration_ms: 2000,
    egress: { selected: { kind: 'direct', group_id: null, group_version: null, group_name: null, member_id: null, member_name: null, scheme: null, host: null, port: null }, observed_at: 1760000000000 } },
  { ...base, request_id: 'request-e', model: 'model-e', status_code: 200, completed_at: 3000, duration_ms: 2000,
    egress: { selected: { kind: 'unknown', group_id: null, group_version: null, group_name: null, member_id: null, member_name: null, scheme: null, host: null, port: null } } },
  { ...base, request_id: 'request-f', model: 'model-f', status_code: 502, completed_at: 3000, duration_ms: 2000, error_code: 'http_502', terminal_cause_code: 'gateway_pre_admission' },
  { ...base, request_id: 'request-g', model: 'model-g', status_code: 200, completed_at: 3000, duration_ms: 2000, egress: null },
];
let hold = false;
let release: (() => void) | undefined;
let holdQuery = false;
let failQuery: (() => void) | undefined;
const json = (value: unknown) => new Response(JSON.stringify(value), { headers: { 'Content-Type': 'application/json' } });
window.fetch = async (input) => {
  const url = new URL(String(input), location.origin);
  if (url.pathname === '/internal/v1/upstreams') {
    if (new URLSearchParams(location.search).has('egress')) return json([{ id: 'account-a', name: 'Research account' }]);
    if (new URLSearchParams(location.search).has('directory-error')) {
      return new Response('secret-directory-body-canary', { status: 503, headers: { 'x-mtc-request-id': '01900000-0000-7000-8000-000000000001' } });
    }
    return json([]);
  }
  if (url.pathname === '/internal/v1/requests/query') {
    window.requestLifecycleFixture.queryCalls += 1;
    if (holdQuery) {
      holdQuery = false; window.requestLifecycleFixture.queryHeld = true;
      return new Promise<Response>((resolve) => { failQuery = () => { window.requestLifecycleFixture.queryHeld = false; resolve(new Response(JSON.stringify({ error: 'fixture query failure' }), { status: 503 })); }; });
    }
    return json({ requests: new URLSearchParams(location.search).has('egress-lines') ? [first, second, ...egressLineRows] : [first, second], next_cursor: { before_created_at: 1000, before_id: second.request_id } });
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
    queryCalls: window.requestLifecycleFixture?.queryCalls ?? 0,
    scopeCommits: window.requestLifecycleFixture?.scopeCommits ?? [],
    queryHeld: window.requestLifecycleFixture?.queryHeld ?? false,
    filter: () => { holdQuery = true; setDrilldown({ ast: { logical_operator: 'and', conditions: [{ field: 'model', operator: 'equals', value: { type: 'model', value: 'different-model' } }] }, revision: 1 }); },
    failQuery: () => { failQuery?.(); },
    hold: () => { hold = true; }, release: () => { release?.(); }, switchScope: () => {
      if (operational) { first = operational; setEvents(new Map()); }
      setTenant('tenant-b');
    },
    settle: (cost, kind) => {
      if (!first.operational_terminal) throw new Error('Operational fixture required');
      first = { ...first, cost, completed_at: 4000, input_tokens: 20, output_tokens: 10, usage_basis: 'provider_reported',
        operational_terminal: { ...first.operational_terminal, financial_state: 'applied' } };
      if (kind === 'archive_bound') {
        const scheduled: { flush?: () => void } = {};
        const batch = new RequestRefreshBatch(5000, {
          schedule: callback => { scheduled.flush = callback; return 1; },
          cancel: () => { scheduled.flush = undefined; },
        }, (published, overflow) => {
          if (overflow) throw new Error('Low-volume fixture must not overflow');
          setEvents(previous => {
            const merged = new Map(previous);
            for (const [requestId, event] of published) merged.set(requestId, coalesceRequestEvent(previous.get(requestId), event));
            return merged;
          });
          setRevision(value => value + 1);
        });
        batch.enqueue({ ...base, status_code: 200, completed_at: 4000, cost: '999',
          event_id: `financial-${revision}`, event_at: 4000, event_kind: 'finished', key_id: 'key' });
        batch.enqueue({ ...base, event_id: `archive-${revision}`, event_at: 4001,
          event_kind: 'archive_bound', archive_state: 'bound', key_id: 'key' });
        scheduled.flush?.();
        batch.dispose();
        return;
      }
      setEvents(new Map([[first.request_id, { ...base, status_code: 200, completed_at: 4000, cost: '999',
        event_id: `financial-${revision}`, event_at: 4000, event_kind: kind, key_id: 'key' } as RequestEvent]]));
      setRevision(value => value + 1);
    },
    replay: () => {
      setEvents(new Map([[first.request_id, { ...base, status_code: 200, completed_at: 2000, cost: '999',
        event_id: 'old-financial', event_at: 2000, event_kind: 'finished', key_id: 'key' } as RequestEvent]]));
      setRevision(value => value + 1);
    },
    finish: () => {
      const finished = supplierFailure ?? { ...base, compaction: true, status_code: 200, completed_at: 3000, duration_ms: 2000, input_tokens: 20, output_tokens: 10, archive_complete: true };
      first = operational ?? finished;
      setEvents(new Map([[first.request_id, { ...finished, event_id: `terminal-${revision}`, event_at: 3000, event_kind: 'finished', key_id: 'key' } as RequestEvent]]));
      setRevision((value) => value + 1);
    },
  };
  useLayoutEffect(() => {
    window.requestLifecycleFixture.scopeCommits.push({ scope: tenant, drawers: document.querySelectorAll('.drawer').length });
  }, [tenant]);
  return <div data-request-fixture-scope={tenant} data-request-fixture-revision={revision} data-request-fixture-event-kind={events.get(base.request_id)?.event_kind} data-request-fixture-completed-at={events.get(base.request_id)?.completed_at}><RequestsPage token="fixture-token" tenant={tenant} liveEvents={events} streamRevision={revision} streamState="live" streamError="" onOpenSessions={() => {}} onOpenSession={() => {}} requestDrilldown={drilldown} requestRefresh={operationalMode ? { intervalMs: 5000, paused: false, onIntervalChange: () => {} } : undefined} /><Tooltip content="Background scope help" visible={backgroundHelpVisible} relationship="description"><button id="background-help">Background helper</button></Tooltip>{revision > 0 && <Tooltip content="Live-added background help" visible={backgroundHelpVisible} relationship="description"><button>Live-added helper</button></Tooltip>}</div>;
}
createRoot(document.getElementById('root')!).render(<I18nProvider><MtcFluentProvider><Fixture /></MtcFluentProvider></I18nProvider>);
