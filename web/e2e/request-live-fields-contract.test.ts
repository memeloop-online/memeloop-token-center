import assert from 'node:assert/strict';
import test from 'node:test';
import { mergeLiveRequestEvents, mergeRefreshedRequestPage, requestViewFromEvent } from '../src/operator/traffic/requestTraffic.js';
import { coalesceRequestEvent } from '../src/operator/traffic/requestRefresh.js';
import type { RequestEvent, RequestView } from '../src/types.js';

const event: RequestEvent = {
  event_id: 'event', request_id: 'request', event_at: 900, event_kind: 'finished',
  key_id: 'key', protocol: 'openai', model: 'model', status_code: 200,
  duration_ms: 400, input_tokens: 100, output_tokens: 20, cost: '1', error_code: null,
  archive_state: 'pending',
};
const recorded: RequestView = {
  request_id: 'request', created_at: 100, completed_at: 800,
  protocol: 'openai', model: 'model', status_code: 200, duration_ms: 400,
  input_tokens: 100, output_tokens: 20, cost: '1', error_code: null,
  terminal_cause_code: null,
  upstream_account_id: 'upstream', route_id: 'route', currency: 'USD',
  cached_input_tokens: 30, cache_write_tokens: 10,
  credential_identity: { tenant_external_id: 'tenant', key_id: 'key', key_alias: 'Research key', principal_external_id: 'research-team' },
  archive_state: 'pending',
  session_context: {
    session_id: 'session', association: 'confirmed', session_name: 'named',
    task_kind: 'task', agent_id: 'agent', semantics_source: 'declared',
  },
};

test('sparse SSE preserves REST exit and confirmed session metadata through terminal updates', () => {
  const egress: NonNullable<RequestView['egress']> = {
    selected: { kind: 'direct', group_id: null, group_version: null, group_name: null,
      member_id: null, member_name: null, scheme: null, host: null, port: null },
    observed_at: 800,
  };
  const enriched = { ...event, created_at: 100, session_context: recorded.session_context };
  const cached = coalesceRequestEvent(undefined, enriched);
  const sparse: RequestEvent = { ...event, event_id: 'later', event_at: 1000,
    event_kind: 'archive_bound', archive_state: 'bound', session_context: { ...recorded.session_context!, association: 'unlinked', session_id: null } };
  const retained = coalesceRequestEvent(cached, sparse);
  const reconstructed = requestViewFromEvent(retained);
  assert.deepEqual(reconstructed?.session_context, recorded.session_context);
  for (const event_kind of ['started', 'projected', 'archive_bound'] as const) {
    for (const operational_terminal of [undefined, { completed_at: 800, stage: null, financial_state: 'pending' as const }]) {
      const previous = { ...recorded, egress, operational_terminal,
        session_context: { ...recorded.session_context!, association: 'unlinked' as const, session_id: null } };
      const merged = requestViewFromEvent({ ...enriched, event_kind }, previous);
      assert.deepEqual(merged?.egress, egress);
      assert.deepEqual(merged?.session_context, recorded.session_context);
      assert.equal(merged?.status_code, previous.status_code);
    }
  }
});

test('retained financial events cannot replace operational outcomes or invent confirmed money', () => {
  for (const status_code of [200, 499, 502]) {
    for (const financial_state of ['pending', 'unknown', 'applied'] as const) {
      const terminal: RequestView = {
        ...recorded, status_code, completed_at: financial_state === 'applied' ? 1000 : null,
        cost: financial_state === 'applied' ? '0' : null,
        error_code: status_code >= 400 ? `http_${status_code}` : null,
        operational_terminal: { completed_at: 800, stage: null, financial_state },
      };
      for (const event_kind of ['started', 'finished', 'projected', 'archive_bound'] as const) {
        const replay: RequestEvent = { ...event, event_kind, completed_at: 700, cost: '99', status_code: 200 };
        const merged = requestViewFromEvent(replay, terminal);
        assert.deepEqual(merged, terminal);
        assert.deepEqual(mergeLiveRequestEvents([terminal], new Map([['request', replay]])), [terminal]);
      }
      const archived = requestViewFromEvent({ ...event, event_kind: 'archive_bound', archive_state: 'bound' }, terminal);
      assert.equal(archived?.archive_state, 'bound');
      assert.equal(archived?.status_code, status_code);
      const applied: RequestView = { ...terminal, completed_at: 1000, cost: '0',
        operational_terminal: { completed_at: 800, stage: null, financial_state: 'applied' } };
      const refreshed = mergeRefreshedRequestPage([terminal], { requests: [applied], next_cursor: null });
      assert.deepEqual(mergeLiveRequestEvents(refreshed, new Map([['request', event]])), [applied]);
    }
  }
});

test('legacy SSE does not erase recorded routing, completion, currency, cache or session fields', () => {
  assert.deepEqual(requestViewFromEvent(event, recorded), recorded);
  const start: RequestEvent = { ...event, event_kind: 'started', status_code: null };
  assert.deepEqual(requestViewFromEvent(start, recorded), recorded);
});

test('finished-only SSE uses receipt and completion facts, never its event publication time', () => {
  const complete: RequestEvent = { ...event, ...recorded };
  assert.deepEqual(requestViewFromEvent(complete), recorded);
  assert.deepEqual(mergeLiveRequestEvents([], new Map([['request', complete]])), [recorded]);
  assert.equal(requestViewFromEvent(event), undefined);
});

test('archive events converge archive state without changing the terminal HTTP status', () => {
  const bound = requestViewFromEvent({ ...event, event_kind: 'archive_bound', archive_state: 'bound' }, recorded);
  assert.equal(bound?.status_code, 200);
  assert.equal(bound?.archive_state, 'bound');
  const replayedFinished = requestViewFromEvent(event, { ...recorded, archive_state: 'bound' });
  assert.equal(replayedFinished?.archive_state, 'bound', 'an older lifecycle event must not regress a bound snapshot');
  const cachedOneSideBound = requestViewFromEvent(
    { ...event, event_kind: 'archive_bound', archive_state: 'pending' },
    { ...recorded, archive_state: 'bound' },
  );
  assert.equal(cachedOneSideBound?.archive_state, 'bound', 'a cached one-side event must not regress a terminal REST snapshot');
  const gapDominatesCachedBound = requestViewFromEvent(
    { ...event, event_kind: 'archive_gap', archive_state: 'gap' },
    { ...recorded, archive_state: 'bound' },
  );
  assert.equal(gapDominatesCachedBound?.archive_state, 'gap');
  const boundCannotHideGap = requestViewFromEvent(
    { ...event, event_kind: 'archive_bound', archive_state: 'bound' },
    { ...recorded, archive_state: 'gap' },
  );
  assert.equal(boundCannotHideGap?.archive_state, 'gap', 'gap dominates when terminal event order is unavailable');
});

test('confirmed session ownership never regresses to an old unlinked event', () => {
  const oldUnlinked = requestViewFromEvent({
    ...event,
    session_context: {
      session_id: null, association: 'unlinked', session_name: null,
      task_kind: null, agent_id: null, semantics_source: null,
    },
  }, recorded);
  assert.deepEqual(oldUnlinked?.session_context, recorded.session_context);

  const unlinkedSnapshot: RequestView = {
    ...recorded,
    session_context: {
      session_id: null, association: 'unlinked', session_name: null,
      task_kind: null, agent_id: null, semantics_source: null,
    },
  };
  const confirmedProjection = requestViewFromEvent({
    ...event,
    event_kind: 'projected',
    session_context: recorded.session_context,
  }, unlinkedSnapshot);
  assert.deepEqual(confirmedProjection?.session_context, recorded.session_context,
    'a confirmed projection advances an unlinked snapshot');
});

test('missing cache telemetry remains absent instead of becoming zero', () => {
  const legacyStart = { ...event, event_kind: 'started' as const, status_code: null };
  const pending = requestViewFromEvent(legacyStart);
  assert.equal(pending?.created_at, event.event_at);
  assert.equal(pending?.cached_input_tokens, undefined);
  assert.equal(pending?.cache_write_tokens, undefined);
  assert.equal(pending?.completed_at, undefined);
  const terminal = requestViewFromEvent({ ...event, cached_input_tokens: 0, cache_write_tokens: 0 }, pending);
  assert.equal(terminal?.cached_input_tokens, 0);
  assert.equal(terminal?.cache_write_tokens, 0);
});

test('legacy SSE without terminal cause preserves the recorded typed cause', () => {
  const failed: RequestView = {
    ...recorded, status_code: 502,
    error_code: 'upstream_eof_without_terminal', terminal_cause_code: 'upstream_eof_without_terminal',
  };
  const start: RequestEvent = { ...event, event_kind: 'started', status_code: null };
  assert.deepEqual(requestViewFromEvent(start, failed), failed);
  assert.equal(requestViewFromEvent(event, failed)?.terminal_cause_code, 'upstream_eof_without_terminal');
  const terminal: RequestEvent = {
    ...event, status_code: 502,
    error_code: 'upstream_eof_without_terminal', terminal_cause_code: 'upstream_eof_without_terminal',
  };
  assert.equal(requestViewFromEvent(terminal, recorded)?.terminal_cause_code, 'upstream_eof_without_terminal');
});
