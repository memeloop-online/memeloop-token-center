import assert from 'node:assert/strict';
import test from 'node:test';
import { confirmationForScope, modelConfirmationValidity } from '../src/operator/modelConfirmation.js';

test('catalog evidence selects one confirmation path and groups cannot bypass missing coverage', () => {
  const defaults = { hasValue: true, catalogListed: false, customAllowed: true, customConfirmed: false };
  for (const [name, input, expected] of [
    ['catalog-listed model', { catalogListed: true }, [false, false, true]],
    ['unknown', {}, [true, false, false]],
    ['confirmed unknown', { customConfirmed: true }, [true, true, true]],
    ['group unknown', { customAllowed: false, customConfirmed: true }, [true, false, false]],
  ] as const) {
    const result = modelConfirmationValidity({ ...defaults, ...input });
    assert.deepEqual([result.needsCustomConfirmation, result.allowCustom, result.valid], expected, name);
  }
});

test('scope changes clear persisted consent immediately and returning never restores it', () => {
  const initial = { scope: 'original', confirmed: true };
  assert.equal(confirmationForScope(initial, 'original'), initial);
  const changed = confirmationForScope(initial, 'changed');
  assert.equal(changed.confirmed, false);
  assert.equal(confirmationForScope(changed, 'original').confirmed, false);
});
