import assert from 'node:assert/strict';
import test from 'node:test';
import { GenerationActionRegistry, startCompletionPolling } from '../src/self/generationConcurrency.js';

test('polling schedules the next tick only after the current request completes', async () => {
  const callbacks = new Map<number, () => void>();
  let nextId = 0;
  let release: (() => void) | undefined;
  const run = () => new Promise<void>((resolve) => { release = resolve; });
  const stop = startCompletionPolling(
    run,
    1_000,
    (callback) => { const id = ++nextId; callbacks.set(id, callback); return id; },
    (id) => { callbacks.delete(id); },
  );

  assert.equal(callbacks.size, 1);
  callbacks.get(1)?.();
  callbacks.delete(1);
  assert.equal(callbacks.size, 0, 'no next timer may exist while refresh is in flight');
  release?.();
  await Promise.resolve();
  await Promise.resolve();
  assert.equal(callbacks.size, 1, 'the next timer is scheduled after refresh completes');
  stop();
  assert.equal(callbacks.size, 0);
});

test('slow cancellation and another job action remain isolated from polling', () => {
  const actions = new GenerationActionRegistry();
  const first = actions.begin('cancel:job-a');
  const second = actions.begin('cancel:job-b');
  assert.ok(first);
  assert.ok(second);
  assert.notEqual(first, second);
  assert.equal(actions.begin('cancel:job-a'), undefined, 'double-click cannot start a duplicate cancellation');
  assert.equal(first.signal.aborted, false);
  assert.equal(second.signal.aborted, false);
  actions.finish('cancel:job-a', first);
  assert.equal(second.signal.aborted, false, 'finishing job A must not abort job B');
  actions.abortAll();
  assert.equal(second.signal.aborted, true, 'credential scope cleanup aborts remaining actions');
});
