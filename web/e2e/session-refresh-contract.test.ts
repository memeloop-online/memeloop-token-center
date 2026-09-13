import assert from 'node:assert/strict';
import test from 'node:test';
import {
  drainSessionEventIdentities, enqueueSessionEventIdentity, mergeSessionPage, sessionEventsRequireDetailRefresh,
  sessionEventTargetsSelection,
} from '../src/operator/sessionRefresh.js';

test('one SSE chunk preserves exact credential and session scopes before React renders', () => {
  const queued = new Set<string>();
  enqueueSessionEventIdentity(queued, {
    key_id: 'key-a', request_id: 'request-a', event_kind: 'finished', status_code: 200,
    session_context: { association: 'confirmed', session_id: 'session-a' },
  });
  enqueueSessionEventIdentity(queued, {
    key_id: 'key-b', request_id: 'request-b', event_kind: 'finished', status_code: 200,
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
    key_id: 'key-a', request_id: 'request-a', event_kind: 'finished', status_code: 200,
    session_context: { association: 'unlinked', session_id: null },
  });
  enqueueSessionEventIdentity(queued, {
    key_id: 'key-b', request_id: 'request-b', event_kind: 'finished', status_code: 200, session_context: null,
  });

  const batch = drainSessionEventIdentities(queued);
  assert.equal(sessionEventTargetsSelection(batch, { key_id: 'key-a', session_id: 'unlinked:key-a' }), true);
  assert.equal(sessionEventTargetsSelection(batch, { key_id: 'key-b', session_id: 'unlinked:key-b' }), false,
    'an event without a tenant-owned session projection may refresh the list but not guess a selected detail');
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
