import assert from 'node:assert/strict';
import test from 'node:test';
import { modelConfirmationValidity } from '../src/operator/modelConfirmation.js';

test('unresolved catalog evidence neither accuses a model nor permits custom bypass', () => {
  for (const catalogListed of [false, true]) {
    for (const customConfirmed of [false, true]) {
      assert.deepEqual(modelConfirmationValidity({ hasValue: true, catalogListed, customAllowed: true, customConfirmed, catalogVerified: false }), {
        needsCustomConfirmation: false, allowCustom: false, valid: false,
      });
    }
  }
});

test('settled catalog preserves discovered-model restrictions and explicit custom consent', () => {
  assert.deepEqual(modelConfirmationValidity({ hasValue: true, catalogListed: true, customAllowed: true, customConfirmed: true, catalogVerified: true }), {
    needsCustomConfirmation: false, allowCustom: false, valid: true,
  });
  for (const customAllowed of [false, true]) {
    for (const customConfirmed of [false, true]) {
      const allowed = customAllowed && customConfirmed;
      assert.deepEqual(modelConfirmationValidity({ hasValue: true, catalogListed: false, customAllowed, customConfirmed, catalogVerified: true }), {
        needsCustomConfirmation: true, allowCustom: allowed, valid: allowed,
      });
    }
  }
});
