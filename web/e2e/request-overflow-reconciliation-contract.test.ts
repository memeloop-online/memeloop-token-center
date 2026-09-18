import assert from 'node:assert/strict';
import test from 'node:test';

import {
  RequestOverflowReconciliation,
  requestOverflowReconcileCooldownMs,
  requestOverflowReconcileDelayMs,
} from '../src/operator/traffic/requestOverflowReconciliation.js';

function clock() {
  let now = 0;
  let nextId = 0;
  const timers = new Map<number, { at: number; run: () => void }>();
  return {
    now: () => now,
    schedule(run: () => void, delay: number) { timers.set(++nextId, { at: now + delay, run }); return nextId; },
    cancel(timer: number) { timers.delete(timer); },
    advance(ms: number) {
      now += ms;
      for (const [id, timer] of [...timers].sort((left, right) => left[1].at - right[1].at)) {
        if (timer.at > now) continue;
        timers.delete(id);
        timer.run();
      }
    },
    get size() { return timers.size; },
  };
}

test('overflow edges collapse into cooldown-bounded authoritative trailing queries', () => {
  const time = clock();
  const started: number[] = [];
  const reconciliation = new RequestOverflowReconciliation(time, ticket => started.push(ticket));
  assert.equal(requestOverflowReconcileCooldownMs, 30_000, 'the reconciliation rate limit is an explicit contract');

  for (let index = 0; index < 1_000; index++) reconciliation.signal();
  assert.equal(time.size, 1, 'a burst owns one fixed edge timer');
  time.advance(requestOverflowReconcileDelayMs - 1);
  assert.deepEqual(started, []);
  time.advance(1);
  assert.deepEqual(started, [1]);

  for (let index = 0; index < 1_000; index++) reconciliation.signal();
  assert.equal(time.size, 0, 'in-flight overflow is one sticky bit, not another timer per signal');
  reconciliation.finish(1, true);
  assert.equal(time.size, 1, 'all in-flight overflow produces exactly one trailing timer');
  for (let index = 0; index < 1_000; index++) reconciliation.signal();
  assert.equal(time.size, 1, 'new overflow cannot slide or multiply the cooldown timer');

  time.advance(requestOverflowReconcileCooldownMs - 1);
  assert.deepEqual(started, [1]);
  time.advance(1);
  assert.deepEqual(started, [1, 2]);
  for (let index = 0; index < 1_000; index++) reconciliation.signal();
  reconciliation.finish(2, true);
  time.advance(requestOverflowReconcileCooldownMs - 1);
  assert.deepEqual(started, [1, 2], 'overflow sustained through the second query still observes the cooldown');
  time.advance(1);
  assert.deepEqual(started, [1, 2, 3], 'the second query retains only one final necessary reconciliation');
  reconciliation.finish(3, true);
  time.advance(requestOverflowReconcileCooldownMs * 2);
  assert.deepEqual(started, [1, 2, 3], 'a clean final pass ends the catch-up episode');
});

test('failure stays sticky but retries only after the hard cooldown', () => {
  const time = clock();
  const started: number[] = [];
  const reconciliation = new RequestOverflowReconciliation(time, ticket => started.push(ticket));

  reconciliation.signal();
  time.advance(requestOverflowReconcileDelayMs);
  reconciliation.finish(1, false);
  time.advance(requestOverflowReconcileCooldownMs - 1);
  assert.deepEqual(started, [1]);
  time.advance(1);
  assert.deepEqual(started, [1, 2]);
});

test('a claimed ticket deferred by a page eligibility race remains dirty until its lane reopens', () => {
  const time = clock();
  const started: number[] = [];
  const reconciliation = new RequestOverflowReconciliation(time, ticket => started.push(ticket));

  reconciliation.signal();
  time.advance(requestOverflowReconcileDelayMs);
  assert.deepEqual(started, [1]);

  // This models the timer claiming the ticket immediately before React commits
  // a foreground load, pause, or filter. It must not be recorded as success.
  reconciliation.defer(1);
  for (let index = 0; index < 1_000; index++) reconciliation.signal();
  time.advance(10_000);
  assert.deepEqual(started, [1], 'a blocked page schedules no background queries');
  assert.equal(time.size, 0, 'the dirty burst remains one sticky edge while blocked');

  reconciliation.setBlocked(false);
  time.advance(requestOverflowReconcileCooldownMs - 10_000 - 1);
  assert.deepEqual(started, [1], 'reopen still honors the completed-pass cooldown');
  time.advance(1);
  assert.deepEqual(started, [1, 2], 'the retained edge receives one authoritative retry');
});

test('blocking and interrupted pagination preserve one dirty edge without wall-clock races', () => {
  const time = clock();
  const started: number[] = [];
  const reconciliation = new RequestOverflowReconciliation(time, ticket => started.push(ticket));

  reconciliation.setBlocked(true);
  for (let index = 0; index < 100; index++) reconciliation.signal();
  time.advance(requestOverflowReconcileCooldownMs * 2);
  assert.deepEqual(started, []);
  assert.equal(time.size, 0);

  reconciliation.setBlocked(false);
  time.advance(requestOverflowReconcileDelayMs);
  assert.deepEqual(started, [1]);
  reconciliation.setBlocked(true);
  reconciliation.reset(true);
  reconciliation.finish(1, true);
  time.advance(requestOverflowReconcileCooldownMs * 2);
  assert.deepEqual(started, [1], 'the stale completion cannot consume the preserved edge');

  reconciliation.setBlocked(false);
  time.advance(requestOverflowReconcileDelayMs);
  assert.deepEqual(started, [1, 2]);
});
