import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import { requestFailureCause, requestStatusCopy } from '../src/requestStatusPresentation.js';
import { requestViewFromEvent } from '../src/operator/traffic/requestTraffic.js';
import type { RequestView, RequestEvent } from '../src/types.js';

const components = await readFile(new URL('../src/components.tsx', import.meta.url), 'utf8');
const selfDrawers = await readFile(new URL('../src/self/SelfDrawers.tsx', import.meta.url), 'utf8');
const selfPortal = await readFile(new URL('../src/self/SelfPortal.tsx', import.meta.url), 'utf8');
const operatorRequests = await readFile(new URL('../src/operator/pages/RequestsPage.tsx', import.meta.url), 'utf8');
const types = await readFile(new URL('../src/types.ts', import.meta.url), 'utf8');
const copyButton = await readFile(new URL('../src/CopyButton.tsx', import.meta.url), 'utf8');
const fixture = await readFile(new URL('./fixtures/request-diagnostics.tsx', import.meta.url), 'utf8');

test('request table exposes copyable durable IDs and its recorded token billing split', () => {
  assert.match(components, /RequestIdentifier/);
  assert.match(components, /request\.request_id/);
  assert.match(components, /<CopyButton value=\{requestId\} label=\{t\('common\.copy'\)\}/);
  assert.match(copyButton, /navigator\.clipboard\?\.writeText/);
  assert.match(components, /request\.tokenBreakdown/);
  assert.match(components, /requestTokenBreakdown/);
  assert.match(components, /const cachedInputTokens = request\.cached_input_tokens;/);
  assert.match(components, /const cacheWriteTokens = request\.cache_write_tokens;/);
  assert.match(components, /if \(cachedInputTokens === undefined \|\| cacheWriteTokens === undefined\) return undefined;/);
  assert.doesNotMatch(components, /cached_input_tokens \?\? 0/);
  assert.doesNotMatch(components, /cache_write_tokens \?\? 0/);
});

test('both Requests drawers reuse the durable request diagnostics surface and session drilldown', () => {
  assert.match(selfDrawers, /<RequestDiagnostics request=\{detail\}/);
  assert.match(selfDrawers, /onOpenSession/);
  assert.match(selfPortal, /setRequestDetail\(undefined\); openSession\(sessionId\)/);
  assert.match(operatorRequests, /<RequestDiagnostics request=\{detail\} onOpenSession=\{onOpenSession\}/);
  assert.match(components, /context\.association === 'confirmed'/);
  assert.match(components, /context\.session_id/);
  assert.match(components, /sessions\.unlinkedRequests/);
});

test('request diagnostics fixture preserves the Shell grid placement used by the browser contract', () => {
  assert.match(fixture, /className="app-shell" data-fixture-ready="request-diagnostics"><aside className="rail" aria-hidden="true" \/><main className="main">/);
  assert.match(fixture, /data-fixture-request="recorded"/);
  assert.match(fixture, /data-fixture-request="historical-gap"/);
});

test('request diagnostics use only nullable server-recorded final routing fields', () => {
  const requestView = types.slice(types.indexOf('export interface RequestView'), types.indexOf('/** Exclusive descending keyset cursor'));
  for (const field of ['completed_at?: number | null', 'upstream_account_id?: string | null', 'route_id?: string | null', 'currency?: string | null']) {
    assert.match(requestView, new RegExp(field.replaceAll('?', '\\?').replaceAll('|', '\\|')));
  }
  assert.match(components, /request\.receivedAt/);
  assert.match(components, /request\.completedAt/);
  assert.match(components, /request\.upstreamId/);
  assert.match(components, /request\.routeId/);
  assert.match(components, /request\.currency === undefined \? fallbackCurrency : request\.currency/);
  assert.doesNotMatch(components, /routing_attempts|ttft|tokens_per_second/i);
});

test('request failure presentation shares the allowlisted recorded cause and never renders raw codes', () => {
  assert.match(components, /const failureCause = pending \|\| requestStatusCopy\(request, locale\)\.cause \? null : requestFailureCause\(request, locale\) \?\? \(request\.error_code \? requestErrorCopy\(request\.error_code, locale\) : null\);/);
  assert.match(components, /\{failureCause && <div className="request-detail-wide"><b>\{t\('request\.error'\)\}<\/b><span>\{failureCause\}<\/span><\/div>\}/);
  assert.doesNotMatch(components, /\{request\.error_code\}/, 'raw recorded error codes never render as copy');
  assert.doesNotMatch(components, /traffic\.errorCode/, 'raw recorded error codes never appear in tooltips');
  assert.match(components, /<td className="request-status-cell" data-label=\{t\('request\.status'\)\}><RequestStatus request=\{request\} \/><\/td>/, 'the status cell relies on the shared status cause presentation');
});

test('supplier reasons share localized tooltip/detail copy, preserve transport precedence and survive refresh', () => {
  const safe: RequestView = { request_id: 'supplier-fixture', created_at: 1, completed_at: 2,
    protocol: 'openai', model: 'fixture', status_code: 402, duration_ms: 1,
    input_tokens: 0, output_tokens: 0, cost: '0', error_code: 'http_402',
    supplier_error: { code: 'no_active_plan', message: 'untrusted-message-canary' } };
  for (const [locale, reason, prefix] of [
    ['zh-CN', '当前上游账号没有可用套餐，需在提供商处开通或更换账号。', '已记录原因'],
    ['en', 'The upstream account has no active plan. Activate a plan with the provider or use another account.', 'Recorded cause'],
  ] as const) {
    const copy = requestStatusCopy(safe, locale);
    assert.equal(requestFailureCause(safe, locale), reason);
    assert.equal(copy.hint, `${prefix}: ${reason}`);
    assert.equal(copy.cause, copy.hint);
    assert.doesNotMatch(copy.hint, /untrusted-message-canary|no_active_plan|http_402|402|请求失败|The request failed/);
    const unknown = requestStatusCopy({ ...safe, supplier_error: { code: 'private-canary', message: reason } }, locale);
    assert.equal(unknown.hint, requestStatusCopy({ ...safe, supplier_error: null }, locale).hint);
    assert.equal(requestStatusCopy({ ...safe, supplier_error: undefined }, locale).hint, unknown.hint);
    assert.equal(requestFailureCause({ ...safe, status_code: null, error_code: null }, locale), null);
  }
  assert.equal(requestFailureCause({ ...safe, terminal_cause_code: 'upstream_read_timeout' }, 'zh-CN'), '读取上游响应超时');
  assert.equal(requestFailureCause({ ...safe, error_code: 'upstream_stream_read_error' }, 'en'), 'The upstream response stream was interrupted');
  const event: RequestEvent = { ...safe, event_id: 'finished', event_at: 2, event_kind: 'finished', key_id: 'key', archive_state: 'bound' };
  const running = { ...safe, status_code: null, completed_at: null, error_code: null, supplier_error: null };
  assert.deepEqual(requestViewFromEvent(event, running)?.supplier_error, safe.supplier_error);
  const partial = { ...event, event_id: 'archive', event_kind: 'archive_bound' } as RequestEvent;
  delete partial.supplier_error;
  assert.deepEqual(requestViewFromEvent(partial, safe)?.supplier_error, safe.supplier_error);
  assert.deepEqual(requestViewFromEvent({ ...partial, supplier_error: null }, safe)?.supplier_error, safe.supplier_error);
  assert.deepEqual(requestViewFromEvent({ ...event, event_kind: 'started', status_code: null }, safe)?.supplier_error, safe.supplier_error);
  assert.equal(requestViewFromEvent({ ...event, supplier_error: null })?.supplier_error, null);
});
