import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const managementPages = await readFile(new URL('../src/operator/pages/ManagementPages.tsx', import.meta.url), 'utf8');

test('existing credential values are never recovered; rotation opens a one-time copy panel', () => {
  assert.doesNotMatch(managementPages, /credential-recovery\/copy/);
  assert.doesNotMatch(managementPages, /copyRecoveredCredential/);
  assert.match(managementPages, /rotateCredential = async/);
  assert.match(managementPages, /credentials\.rotateToCopy/);
  assert.match(managementPages, /filename="client-credential\.txt"/);
  assert.match(managementPages, /onDismiss=\{\(\) => setSecret\('\'\)\}/);
});
