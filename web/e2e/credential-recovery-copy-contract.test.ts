import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const [managementPages, recoveryDb, recoveryApi, recoveryMigration, openapi] = await Promise.all([
  readFile(new URL('../src/operator/pages/ManagementPages.tsx', import.meta.url), 'utf8'),
  readFile(new URL('../../src/db/credentials/recovery.rs', import.meta.url), 'utf8'),
  readFile(new URL('../../src/api/credentials/client.rs', import.meta.url), 'utf8'),
  readFile(new URL('../../migrations/common/0075_key_credential_recovery_access_limits.sql', import.meta.url), 'utf8'),
  readFile(new URL('../../openapi/openapi.yaml', import.meta.url), 'utf8'),
]);

test('only envelope-backed existing credentials can be explicitly confirmed and recovered', () => {
  assert.match(managementPages, /credential_recovery_available/);
  assert.match(managementPages, /recoverCredential = async/);
  assert.match(managementPages, /await confirm\(t\('credentials\.confirmRecovery'/);
  assert.match(managementPages, /credential-recovery\/copy/);
  assert.match(managementPages, /result\.key_id !== value\.key_id \|\| result\.credential_generation !== value\.credential_generation/);
  assert.match(managementPages, /showSecret\(\{ value: result\.key, recovered: true, displayId: crypto\.randomUUID\(\) \}\)/);
  assert.match(managementPages, /const secretResponseRequestPolicy = \{[\s\S]*cache: 'no-store',[\s\S]*credentials: 'omit',[\s\S]*referrerPolicy: 'no-referrer'/);
  assert.match(managementPages, /credential-recovery\/copy[\s\S]*\.\.\.secretResponseRequestPolicy, method: 'POST', signal: controller\.signal/);
  assert.match(managementPages, /secretRequest\.current\?\.abort\(\)/);
  assert.match(managementPages, /secretOperation\.current/);
  assert.match(managementPages, /secret\?\.scopeGeneration === renderScope\.current\.generation/, 'plaintext from the previous auth or tenant scope must not render during a scope transition');
  assert.match(managementPages, /credentials\.recoverAndCopy/);
  assert.match(managementPages, /credentials\.copyNotStored/);
  assert.match(managementPages, /credentials\.rotateToCopy/);
  assert.match(managementPages, /filename="client-credential\.txt"/);
  assert.match(managementPages, /key=\{visibleSecret\.displayId\}/, 'the remount key must not contain the secret');
  assert.match(managementPages, /onDismiss=\{dismissSecret\}/);
});

test('credential recovery is tenant and actor scoped, durably rate limited, audited, and no-store', () => {
  assert.match(recoveryApi, /authenticated_service\(&headers, &state\)/);
  assert.match(recoveryApi, /service\.allows\("keys:write"\)/);
  assert.match(recoveryDb, /actor_tenant_external_id/);
  assert.match(recoveryDb, /KEY_CREDENTIAL_RECOVERY_TENANT_ACTOR_LIMIT/);
  assert.match(recoveryDb, /KEY_CREDENTIAL_RECOVERY_KEY_LIMIT/);
  assert.match(recoveryDb, /KEY_CREDENTIAL_RECOVERY_TENANT_BUCKET/);
  assert.match(recoveryDb, /consume_key_credential_recovery_rate_limit/);
  assert.match(recoveryDb, /"scope_denied"/);
  assert.match(recoveryDb, /"tenant_denied"/);
  assert.match(recoveryDb, /"rate_limited"/);
  assert.match(recoveryDb, /"integrity_failed"/);
  assert.match(recoveryDb, /record_key_credential_recovery_access_audit/);
  assert.match(recoveryMigration, /PRIMARY KEY \(tenant_id, actor_id, bucket_key\)/);
  assert.match(recoveryMigration, /key_credential_recovery_access_audit/);
  assert.match(recoveryMigration, /actor_type TEXT NOT NULL/);
  assert.doesNotMatch(recoveryMigration, /ciphertext|secret_hash|fingerprint|credential\s+TEXT/i);
  assert.match(openapi, /prevent bulk plaintext recovery across keys/);
  assert.match(openapi, /'429':/);
  assert.match(recoveryApi, /CACHE_CONTROL, HeaderValue::from_static\("no-store"\)/);
});
