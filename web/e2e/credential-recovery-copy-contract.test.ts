import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const [managementPages, types] = await Promise.all([
  readFile(new URL('../src/operator/pages/ManagementPages.tsx', import.meta.url), 'utf8'),
  readFile(new URL('../src/types.ts', import.meta.url), 'utf8'),
]);

test('credential recovery is explicit, clipboard-confirmed, and never part of a list payload', () => {
  assert.match(types, /credential_recovery_available\?: boolean/);
  assert.match(managementPages, /credential_recovery_available && value\.status === 'active'/);
  assert.match(managementPages, /\/internal\/v1\/keys\/\$\{value\.key_id\}\/credential-recovery\/copy/);
  assert.match(managementPages, /method: 'POST'/);
  assert.match(managementPages, /await navigator\.clipboard\.writeText\(result\.key\)/);
  assert.match(managementPages, /setMessage\(t\('credentials\.copySuccess'/);
  assert.match(managementPages, /setManualRecoverySecret\(\{ keyId: value\.key_id, key: result\.key \}\)/);
  assert.match(managementPages, /credential-recovery-manual/);
  assert.doesNotMatch(managementPages, /localStorage\.(?:setItem|getItem).*manualRecoverySecret/s);
  assert.doesNotMatch(managementPages, /sessionStorage\.(?:setItem|getItem).*manualRecoverySecret/s);
});
