import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const managementPages = await readFile(new URL('../src/operator/pages/ManagementPages.tsx', import.meta.url), 'utf8');

test('only envelope-backed existing credentials can be explicitly confirmed and recovered', () => {
  assert.match(managementPages, /credential_recovery_available/);
  assert.match(managementPages, /recoverCredential = async/);
  assert.match(managementPages, /await confirm\(t\('credentials\.confirmRecovery'/);
  assert.match(managementPages, /credential-recovery\/copy/);
  assert.match(managementPages, /result\.key_id !== value\.key_id \|\| result\.credential_generation !== value\.credential_generation/);
  assert.match(managementPages, /setSecret\(\{ value: result\.key, recovered: true, displayId: crypto\.randomUUID\(\) \}\)/);
  assert.match(managementPages, /credentials\.recoverAndCopy/);
  assert.match(managementPages, /credentials\.copyNotStored/);
  assert.match(managementPages, /credentials\.rotateToCopy/);
  assert.match(managementPages, /filename="client-credential\.txt"/);
  assert.match(managementPages, /key=\{secret\.displayId\}/, 'the remount key must not contain the secret');
  assert.match(managementPages, /onDismiss=\{\(\) => setSecret\(undefined\)\}/);
});
