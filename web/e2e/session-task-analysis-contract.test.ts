import assert from 'node:assert/strict';
import test from 'node:test';
import { microsToCostString } from '../src/operator/sessionTasks.js';

test('task rollup micros convert to decimal strings without float error', () => {
  assert.equal(microsToCostString(0), '0');
  assert.equal(microsToCostString(12_340_000), '12.34');
  assert.equal(microsToCostString(1), '0.000001');
  assert.equal(microsToCostString(1_000_000), '1');
  assert.equal(microsToCostString(-250_000), '-0.25');
  assert.equal(microsToCostString(1_500_000_000), '1500');
});
