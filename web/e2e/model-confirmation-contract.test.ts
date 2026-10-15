import assert from 'node:assert/strict';
import test from 'node:test';
import { confirmationForScope, modelConfirmationValidity } from '../src/operator/modelConfirmation.js';

test('explicit model selection replaces the checkbox without silently widening existing policies', () => {
  const defaults = { hasValue: true, catalogListed: false, customConfirmed: false };
  for (const [name, input, expected] of [
    ['catalog-listed model', { catalogListed: true }, [false, false, true]],
    ['unknown without selection', {}, [true, false, false]],
    ['unscoped old confirmation', { customConfirmed: true }, [true, false, false]],
    ['entered custom model', { selection: 'custom', customConfirmed: true, catalogVerified: false }, [true, true, true]],
    ['selected catalog model during read failure', { selection: 'catalog', catalogVerified: false }, [true, false, true]],
    ['unchanged restricted route during failure', { selection: 'existing', catalogVerified: false }, [true, false, true]],
    ['unchanged custom route during failure', { selection: 'existing', customConfirmed: true, catalogVerified: false }, [true, true, true]],
    ['unchanged custom route remains custom when listed', { selection: 'existing', customConfirmed: true, catalogListed: true }, [false, true, true]],
    ['empty is never valid', { hasValue: false, selection: 'custom', customConfirmed: true }, [false, false, false]],
  ] as const) {
    const result = modelConfirmationValidity({ ...defaults, ...input });
    assert.deepEqual([result.unverified, result.allowCustom, result.valid], expected, name);
  }
});

test('scope changes clear persisted consent immediately and returning never restores it', () => {
  const initial = { scope: 'original', confirmed: true, selection: 'existing' as const };
  assert.equal(confirmationForScope(initial, 'original'), initial);
  const changed = confirmationForScope(initial, 'changed');
  assert.equal(changed.confirmed, false);
  assert.equal(changed.selection, undefined);
  assert.equal(confirmationForScope(changed, 'original').confirmed, false);
});
