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
      assert.deepEqual(modelConfirmationValidity({ hasValue: true, catalogListed, customConfirmed, catalogVerified: catalogEvidenceVerified(catalog) }), {
        unverified: true, allowCustom: false, valid: false,
      });
    }
  }
  const stale = { eligible_account_count: 2, unknown_account_count: 0, stale_account_count: 2 };
  assert.equal(catalogEvidenceVerified(stale, 2), true, 'current-generation stale snapshots remain evidence');
  assert.equal(catalogEvidenceVerified(stale, 3), false, 'an omitted explicit candidate is not verified coverage');
  assert.equal(catalogEvidenceVerified(undefined), false);
});

test('terminal unsupported discovery is distinguished from incomplete coverage', () => {
  const unsupported = { eligible_account_count: 2, unknown_account_count: 2, unsupported_account_count: 2, stale_account_count: 0 };
  assert.equal(catalogEvidenceVerified(unsupported, 2, true), true);
  assert.equal(catalogEvidenceVerified(unsupported, 2, false), false, 'unsupported discovery does not verify text-model coverage');
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

test('unresolved catalog evidence alone cannot silently authorize a custom model', () => {
  for (const catalogListed of [false, true]) {
    for (const customConfirmed of [false, true]) {
      assert.deepEqual(modelConfirmationValidity({ hasValue: true, catalogListed, customConfirmed, catalogVerified: false }), {
        unverified: true, allowCustom: false, valid: false,
      });
    }
  }
});

test('listed and partial catalog choices stay restricted rather than becoming custom', () => {
  assert.deepEqual(modelConfirmationValidity({ hasValue: true, catalogListed: true, selection: 'custom', customConfirmed: true, catalogVerified: true }), {
    unverified: false, allowCustom: false, valid: true,
  });
  assert.deepEqual(modelConfirmationValidity({ hasValue: true, catalogListed: true, selection: 'catalog', customConfirmed: false, catalogVerified: false }), {
    unverified: true, allowCustom: false, valid: true,
  });
});
