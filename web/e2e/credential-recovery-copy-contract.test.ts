import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const [managementPages, types] = await Promise.all([
  readFile(new URL('../src/operator/pages/ManagementPages.tsx', import.meta.url), 'utf8'),
  readFile(new URL('../src/types.ts', import.meta.url), 'utf8'),
]);

test('existing credential values are never recovered; rotation opens a one-time copy panel', () => {
  assert.match(types, /credential_recovery_available\?: boolean/);
  assert.doesNotMatch(managementPages, /credential-recovery\/copy/);
  assert.doesNotMatch(managementPages, /copyRecoveredCredential/);
  assert.match(managementPages, /rotateCredential = async/);
  assert.match(managementPages, /credentials\.rotateToCopy/);
  assert.match(managementPages, /filename="client-credential\.txt"/);
  assert.match(managementPages, /onDismiss=\{\(\) => setSecret\('\'\)\}/);
});
