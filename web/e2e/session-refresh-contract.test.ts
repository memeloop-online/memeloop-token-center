import assert from 'node:assert/strict';
import test from 'node:test';
import {
  drainSessionEventKeys, enqueueSessionEventKey, mergeSessionPage,
} from '../src/operator/sessionRefresh.js';

test('one SSE chunk preserves every credential scope before React renders', () => {
  const queued = new Set<string>();
  enqueueSessionEventKey(queued, 'key-a');
  enqueueSessionEventKey(queued, 'key-b');

  assert.deepEqual([...drainSessionEventKeys(queued)].sort(), ['key-a', 'key-b']);
  assert.equal(queued.size, 0, 'draining acknowledges exactly the processed batch');
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
