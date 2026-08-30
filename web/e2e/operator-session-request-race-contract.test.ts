import assert from 'node:assert/strict';
import test from 'node:test';
import { LatestRequestGate } from '../src/operator/SessionMonitor.js';

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}

test('deferred detail B supersedes A even when A resolves last', async () => {
  const gate = new LatestRequestGate();
  const a = deferred<string>();
  const b = deferred<string>();
  const requestA = gate.begin();
  const accepted: string[] = [];
  const consumeA = a.promise.then((value) => { if (requestA.isCurrent()) accepted.push(value); });
  const requestB = gate.begin();
  const consumeB = b.promise.then((value) => { if (requestB.isCurrent()) accepted.push(value); });

  assert.equal(requestA.signal.aborted, true);
  b.resolve('B');
  await consumeB;
  a.resolve('A');
  await consumeA;
  assert.deepEqual(accepted, ['B']);
});

test('scope invalidation aborts and rejects a deferred response', async () => {
  const gate = new LatestRequestGate();
  const response = deferred<string>();
  const request = gate.begin();
  let accepted: string | undefined;
  const consume = response.promise.then((value) => { if (request.isCurrent()) accepted = value; });

  gate.invalidate();
  assert.equal(request.signal.aborted, true);
  response.resolve('old-scope');
  await consume;
  assert.equal(accepted, undefined);
});
