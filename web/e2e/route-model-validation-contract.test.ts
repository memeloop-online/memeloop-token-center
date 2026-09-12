import assert from 'node:assert/strict';
import test from 'node:test';
import { catalogEvidenceVerified, modelConfirmationValidity } from '../src/operator/modelConfirmation.js';

test('aggregate evidence counts candidates, not just HTTP success or returned models', () => {
  for (const catalog of [
    { eligible_account_count: 1, unknown_account_count: 1, stale_account_count: 0 },
    { eligible_account_count: 2, unknown_account_count: 1, stale_account_count: 1 },
    { eligible_account_count: 0, unknown_account_count: 0, stale_account_count: 0 },
    { eligible_account_count: 1, unknown_account_count: 0, stale_account_count: 2 },
  ]) {
    for (const customConfirmed of [false, true]) for (const catalogListed of [false, true]) {
      assert.equal(catalogEvidenceVerified(catalog), false);
      assert.deepEqual(modelConfirmationValidity({ hasValue: true, catalogListed, customAllowed: true, customConfirmed, catalogVerified: catalogEvidenceVerified(catalog) }), {
        needsCustomConfirmation: false, allowCustom: false, valid: false,
      });
    }
  }
  const stale = { eligible_account_count: 2, unknown_account_count: 0, stale_account_count: 2 };
  assert.equal(catalogEvidenceVerified(stale, 2), true, 'current-generation stale snapshots remain evidence');
  assert.equal(catalogEvidenceVerified(stale, 3), false, 'an omitted explicit candidate cannot authorize a custom bypass');
  assert.equal(catalogEvidenceVerified(undefined), false);
});

test('only terminal unsupported discovery can exempt pure explicit generation candidates', () => {
  const unsupported = { eligible_account_count: 2, unknown_account_count: 2, unsupported_account_count: 2, stale_account_count: 0 };
  assert.equal(catalogEvidenceVerified(unsupported, 2, true), true);
  assert.equal(catalogEvidenceVerified(unsupported, 2, false), false, 'text and group routes cannot opt into custom unsupported discovery');
  for (const patch of [
    { unsupported_account_count: 1 },
    { unsupported_account_count: 3 },
    { unsupported_account_count: -1 },
    { unsupported_account_count: 0.5 },
    { unknown_account_count: 3 },
    { unknown_account_count: -1 },
    { stale_account_count: 1 },
  ]) assert.equal(catalogEvidenceVerified({ ...unsupported, ...patch }, 2, true), false, JSON.stringify(patch));
});

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
