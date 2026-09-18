import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import {
  drainSessionEventIdentities, enqueueSessionEventIdentity, mergeIncrementalSessionSummaries,
  mergeSessionPage, sessionEventsRequireDetailRefresh, sessionEventTargetsSelection,
  sessionSummaryTargets,
} from '../src/operator/sessionRefresh.js';

test('session monitor uses bounded summary reads and keeps manual refresh authoritative', async () => {
  const monitor = await readFile(new URL('../src/operator/SessionMonitor.tsx', import.meta.url), 'utf8');
  assert.match(monitor, /api<LogicalSessionSummaryBatchResponse>\('\/internal\/v1\/sessions\/summaries'/);
  assert.match(monitor, /method: 'POST'/);
  assert.match(monitor, /if \(forceFullList \|\| targets\.requiresFullReload\)/,
    'overflow and unknown identities retain the full-list fallback');
  assert.match(monitor, /if \(merged\.requiresFullReload\)/,
    'first-page membership uncertainty retains the full-list fallback');
  assert.match(monitor, /async function refreshNow\(\)[\s\S]*loadSessions\(false, filtersRef\.current/,
    'manual refresh remains an authoritative list read');
});

test('one SSE chunk preserves exact credential and session scopes before React renders', () => {
  const queued = new Set<string>();
  enqueueSessionEventIdentity(queued, {
    key_id: 'key-a', request_id: 'request-a', event_kind: 'finished', status_code: 200, archive_state: 'pending',
    session_context: { association: 'confirmed', session_id: 'session-a' },
  });
  enqueueSessionEventIdentity(queued, {
    key_id: 'key-b', request_id: 'request-b', event_kind: 'finished', status_code: 200, archive_state: 'pending',
    session_context: { association: 'confirmed', session_id: 'session-b' },
  });

  const batch = drainSessionEventIdentities(queued);
  assert.equal(batch.size, 2);
  assert.equal(sessionEventTargetsSelection(batch, { key_id: 'key-a', session_id: 'session-a' }), true);
  assert.equal(sessionEventTargetsSelection(batch, { key_id: 'key-a', session_id: 'session-b' }), false,
    'another session on the same credential must not refresh the selected detail');
  assert.equal(sessionEventTargetsSelection(batch, { key_id: 'key-b', session_id: 'session-a' }), false,
    'another credential must not refresh the selected detail');
  assert.equal(sessionEventsRequireDetailRefresh(batch, { key_id: 'key-a', session_id: 'session-a' }, {
    session_id: 'session-a', requests: [{ request_id: 'request-a', status_code: 200 }],
  }), false, 'an event already represented by the opened detail must not issue a redundant refresh');
  assert.equal(sessionEventsRequireDetailRefresh(batch, { key_id: 'key-a', session_id: 'session-a' }, {
    session_id: 'session-a', requests: [{ request_id: 'request-a', status_code: null }],
  }), true, 'a terminal event must refresh a still-pending selected request');
  assert.equal(queued.size, 0, 'draining acknowledges exactly the processed batch');
});

test('unlinked events target only their credential-owned aggregate and unknown context targets no detail', () => {
  const queued = new Set<string>();
  enqueueSessionEventIdentity(queued, {
    key_id: 'key-a', request_id: 'request-a', event_kind: 'finished', status_code: 200, archive_state: 'pending',
    session_context: { association: 'unlinked', session_id: null },
  });
  enqueueSessionEventIdentity(queued, {
    key_id: 'key-b', request_id: 'request-b', event_kind: 'finished', status_code: 200,
    archive_state: 'pending', session_context: null,
  });

  const batch = drainSessionEventIdentities(queued);
  assert.equal(sessionEventTargetsSelection(batch, { key_id: 'key-a', session_id: 'unlinked:key-a' }), true);
  assert.equal(sessionEventTargetsSelection(batch, { key_id: 'key-b', session_id: 'unlinked:key-b' }), false,
    'an event without a tenant-owned session projection may refresh the list but not guess a selected detail');

  const unknownProjection = new Set<string>();
  enqueueSessionEventIdentity(unknownProjection, {
    key_id: 'key-b', request_id: 'request-b', event_kind: 'projected', status_code: 200,
    archive_state: 'pending', session_context: null,
  });
  assert.equal(sessionEventTargetsSelection(
    drainSessionEventIdentities(unknownProjection), { key_id: 'key-b', session_id: 'unlinked:key-b' },
  ), false, 'a projected event without confirmed ownership must not guess an unlinked transition');
});

test('a confirmed projection invalidates only the former same-credential unlinked aggregate', () => {
  const queued = new Set<string>();
  enqueueSessionEventIdentity(queued, {
    key_id: 'key-a', request_id: 'request-a', event_kind: 'projected', status_code: 200, archive_state: 'pending',
    session_context: { association: 'confirmed', session_id: 'confirmed-session' },
  });
  const batch = drainSessionEventIdentities(queued);
  const oldUnlinked = { key_id: 'key-a', session_id: 'unlinked:key-a' };
  assert.equal(sessionEventTargetsSelection(batch, oldUnlinked), true);
  assert.equal(sessionEventsRequireDetailRefresh(batch, oldUnlinked, {
    session_id: oldUnlinked.session_id,
    requests: [{
      request_id: 'request-a', status_code: 200, archive_state: 'pending',
      session_context: { session_id: null, association: 'unlinked', session_name: null, task_kind: null, agent_id: null, semantics_source: null },
    }],
  }), true, 'projection must remove the request from an already-open unlinked aggregate');
  assert.equal(sessionEventTargetsSelection(batch, { key_id: 'key-a', session_id: 'unrelated-session' }), false,
    'projection must not invalidate an unrelated named session on the same credential');
  assert.equal(sessionEventTargetsSelection(batch, { key_id: 'key-b', session_id: 'unlinked:key-b' }), false,
    'projection must not invalidate another credential unlinked aggregate');
});

test('archive transitions refresh an exact selected request even when HTTP status is unchanged', () => {
  const queued = new Set<string>();
  enqueueSessionEventIdentity(queued, {
    key_id: 'key-a', request_id: 'request-a', event_kind: 'archive_bound', status_code: 200, archive_state: 'bound',
    session_context: { association: 'confirmed', session_id: 'session-a' },
  });
  const batch = drainSessionEventIdentities(queued);
  const selected = { key_id: 'key-a', session_id: 'session-a' };
  assert.equal(sessionEventsRequireDetailRefresh(batch, selected, {
    session_id: selected.session_id,
    requests: [{ request_id: 'request-a', status_code: 200, archive_state: 'pending' }],
  }), true, 'archive state, not unchanged HTTP status, drives convergence');
  assert.equal(sessionEventsRequireDetailRefresh(batch, selected, {
    session_id: selected.session_id,
    requests: [{ request_id: 'request-a', status_code: 200, archive_state: 'bound' }],
  }), true, 'the event can describe one spool while combined state already reflects the other spool');

  for (const eventKind of ['archive_bound', 'archive_gap'] as const) {
    const oneSideTransition = new Set<string>();
    enqueueSessionEventIdentity(oneSideTransition, {
      key_id: 'key-a', request_id: 'request-a', event_kind: eventKind, status_code: 200,
      archive_state: 'pending',
      session_context: { association: 'confirmed', session_id: 'session-a' },
    });
    assert.equal(sessionEventsRequireDetailRefresh(
      drainSessionEventIdentities(oneSideTransition), selected,
      { session_id: selected.session_id, requests: [{ request_id: 'request-a', status_code: 200, archive_state: 'pending' }] },
    ), true, `${eventKind} must refresh even while the combined archive state remains pending`);
  }
});

test('an active session beyond the first fifty cannot survive a terminal refresh as a ghost', () => {
  const firstPage = Array.from({ length: 50 }, (_, index) => ({
    key_id: 'key-a', session_id: `active-${String(index).padStart(2, '0')}`,
  }));
  const terminalTail = { key_id: 'key-a', session_id: 'active-tail-now-terminal' };
  const merged = mergeSessionPage({
    current: [...firstPage, terminalTail],
    page: firstPage,
    firstPageSize: 50,
    loadedOlder: true,
    older: false,
    background: true,
    state: 'active',
  });

  assert.equal(merged.sessions.length, 50);
  assert.equal(merged.sessions.some((session) => session.session_id === terminalTail.session_id), false);
  assert.equal(merged.loadedOlder, false, 'the volatile tail must be reloaded from a new server cursor');
});

test('event batches resolve exact summary identities and fail closed for unknown ownership', () => {
  const queued = new Set<string>();
  enqueueSessionEventIdentity(queued, {
    key_id: 'key-a', request_id: 'request-a', event_kind: 'projected', status_code: 200, archive_state: 'bound',
    session_context: { association: 'confirmed', session_id: 'session-a' },
  });
  const projected = sessionSummaryTargets(drainSessionEventIdentities(queued));
  assert.deepEqual(projected, {
    identities: [
      { key_id: 'key-a', session_id: 'session-a' },
      { key_id: 'key-a', session_id: 'unlinked:key-a' },
    ],
    requiresFullReload: false,
  }, 'a projection refreshes both its confirmed destination and former unlinked aggregate');

  enqueueSessionEventIdentity(queued, {
    key_id: 'key-b', request_id: 'request-b', event_kind: 'finished', status_code: 200, archive_state: 'bound',
    session_context: null,
  });
  assert.equal(sessionSummaryTargets(drainSessionEventIdentities(queued)).requiresFullReload, true,
    'an event without a tenant-owned session identity cannot be patched safely');

  enqueueSessionEventIdentity(queued, {
    key_id: 'key-active',
    request_id: 'request-active',
    event_kind: 'finished',
    status_code: 200,
    archive_state: 'bound',
    session_context: { association: 'confirmed', session_id: 'session-active' },
  });
  assert.equal(sessionSummaryTargets(drainSessionEventIdentities(queued), 'active').requiresFullReload, true,
    'one coalesced terminal batch must rebuild an active-filter page after its row can disappear');

  for (let index = 0; index < 101; index += 1) {
    enqueueSessionEventIdentity(queued, {
      key_id: 'key-a', request_id: `request-${index}`, event_kind: 'finished', status_code: 200, archive_state: 'bound',
      session_context: { association: 'confirmed', session_id: `session-${index}` },
    });
  }
  assert.equal(sessionSummaryTargets(drainSessionEventIdentities(queued)).requiresFullReload, true,
    'a batch larger than the bounded summary contract falls back to one list read');
});

test('incremental summaries replace and reorder only affected rows when first-page membership is stable', () => {
  const current = [
    { key_id: 'key-a', session_id: 'session-a', last_activity_at: 300, requests: 1 },
    { key_id: 'key-a', session_id: 'session-b', last_activity_at: 200, requests: 1 },
    { key_id: 'key-a', session_id: 'session-c', last_activity_at: 100, requests: 1 },
  ];
  const updated = { ...current[1], last_activity_at: 350, requests: 2 };
  const merged = mergeIncrementalSessionSummaries({
    current,
    updates: [updated],
    requested: [updated],
    firstPageSize: 3,
    firstPageLimit: 50,
    hasMore: false,
  });
  assert.equal(merged.requiresFullReload, false);
  assert.deepEqual(merged.sessions.map((session) => session.session_id), ['session-b', 'session-a', 'session-c']);
  assert.equal(merged.sessions[0].requests, 2);
  assert.equal(merged.sessions[1], current[0], 'an unaffected row retains its exact object identity');
});

test('incremental summaries use the backend UTF-8 cursor order for equal timestamps', () => {
  const current = [
    { key_id: 'key-a', session_id: '中', last_activity_at: 300, requests: 1 },
    { key_id: 'key-a', session_id: 'a', last_activity_at: 300, requests: 1 },
  ];
  const updated = { ...current[1], requests: 2 };
  const merged = mergeIncrementalSessionSummaries({
    current,
    updates: [updated],
    requested: [updated],
    firstPageSize: 2,
    firstPageLimit: 50,
    hasMore: false,
  });
  assert.equal(merged.requiresFullReload, false);
  assert.deepEqual(merged.sessions.map((session) => session.session_id), ['中', 'a'],
    'UTF-8 byte ordering must match the PostgreSQL cursor rather than browser locale collation');
});

test('incremental summaries reload only when an affected identity can change first-page membership', () => {
  const current = [
    { key_id: 'key-a', session_id: 'session-a', last_activity_at: 300 },
    { key_id: 'key-a', session_id: 'session-b', last_activity_at: 200 },
  ];
  const newRecent = { key_id: 'key-b', session_id: 'session-new', last_activity_at: 400 };
  assert.equal(mergeIncrementalSessionSummaries({
    current,
    updates: [newRecent],
    requested: [newRecent],
    firstPageSize: 2,
    firstPageLimit: 2,
    hasMore: true,
  }).requiresFullReload, true, 'a newly visible identity must refill the authoritative page');

  const oldInvisible = { key_id: 'key-b', session_id: 'session-old', last_activity_at: 10 };
  const ignored = mergeIncrementalSessionSummaries({
    current,
    updates: [oldInvisible],
    requested: [oldInvisible],
    firstPageSize: 2,
    firstPageLimit: 2,
    hasMore: true,
  });
  assert.equal(ignored.requiresFullReload, false);
  assert.deepEqual(ignored.sessions, current, 'an identity remaining below the page boundary does not disturb the list');

  assert.equal(mergeIncrementalSessionSummaries({
    current,
    updates: [oldInvisible],
    requested: [oldInvisible],
    firstPageSize: 2,
    firstPageLimit: 2,
    hasMore: false,
  }).requiresFullReload, true,
    'an unseen row after an exhaustive full page needs a new cursor-bearing snapshot');

  assert.equal(mergeIncrementalSessionSummaries({
    current,
    updates: [],
    requested: [current[0]],
    firstPageSize: 2,
    firstPageLimit: 2,
    hasMore: true,
  }).requiresFullReload, true, 'a visible row removed by state/model/search filters must be backfilled');
});

test('incremental summaries keep the loaded tail globally ordered without changing first-page membership', () => {
  const current = [
    { key_id: 'key-a', session_id: 'session-a', last_activity_at: 400 },
    { key_id: 'key-a', session_id: 'session-b', last_activity_at: 300 },
    { key_id: 'key-a', session_id: 'session-c', last_activity_at: 200 },
    { key_id: 'key-a', session_id: 'session-d', last_activity_at: 100 },
  ];
  const updatedTail = { ...current[3], last_activity_at: 250 };
  const merged = mergeIncrementalSessionSummaries({
    current,
    updates: [updatedTail],
    requested: [updatedTail],
    firstPageSize: 2,
    firstPageLimit: 2,
    hasMore: true,
  });
  assert.equal(merged.requiresFullReload, false);
  assert.deepEqual(merged.sessions.map((session) => session.session_id),
    ['session-a', 'session-b', 'session-d', 'session-c']);
  assert.deepEqual(merged.sessions.slice(0, 2), current.slice(0, 2),
    'tail-only movement must preserve the authoritative first-page membership');
});
