import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const [copyButton, oneTimeSecret, managementPages, portal, settings] = await Promise.all([
  readFile(new URL('../src/CopyButton.tsx', import.meta.url), 'utf8'),
  readFile(new URL('../src/operator/scope/operatorShared.tsx', import.meta.url), 'utf8'),
  readFile(new URL('../src/operator/pages/ManagementPages.tsx', import.meta.url), 'utf8'),
  readFile(new URL('../src/self/SelfPortal.tsx', import.meta.url), 'utf8'),
  readFile(new URL('../src/operator/pages/SystemSettingsPage.tsx', import.meta.url), 'utf8'),
]);
const serviceWorkspace = managementPages.slice(
  managementPages.indexOf('function ServiceCredentialWorkspace'),
  managementPages.indexOf('interface OperatorPageProps'),
);

test('credential copying shares clipboard handling with accessible failure feedback', () => {
  assert.match(copyButton, /navigator\.clipboard\?\.writeText/);
  assert.match(copyButton, /navigator\.clipboard\.writeText\(value\)/);
  assert.match(copyButton, /catch \{\s*setState\('failed'\);/s);
  assert.match(copyButton, /role="status" aria-live="polite"/);
  assert.match(copyButton, /common\.requestFailed/);
  assert.match(oneTimeSecret, /navigator\.clipboard\?\.writeText/);
  assert.match(oneTimeSecret, /document\.execCommand\('copy'\)/);
  assert.match(oneTimeSecret, /role="alert"/);
  assert.match(oneTimeSecret, /role="status"/);
  assert.doesNotMatch(oneTimeSecret, /<aside className="one-time" role=/, 'the secret container itself must never be a live region');
});

test('only current plaintext credential sources are offered for copying', () => {
  assert.match(managementPages, /<OneTimeSecret key=\{visibleSecret\.displayId\} value=\{visibleSecret\.value\}/);
  assert.match(managementPages, /api<\{ key: string; key_id: string \}>\('\/internal\/v1\/keys'/);
  assert.match(managementPages, /showSecret\(\{ value: created\.key, recovered: false, displayId: crypto\.randomUUID\(\) \}\)/);
  assert.match(managementPages, /api<\{ key: string \}>\(`\/internal\/v1\/keys\/\$\{value\.key_id\}\/rotate`/);
  assert.match(managementPages, /showSecret\(\{ value: result\.key, recovered: false, displayId: crypto\.randomUUID\(\) \}\)/);
  assert.match(managementPages, /api<\{ token: string \}>\('\/internal\/v1\/service-tokens'/);
  assert.match(managementPages, /showSecret\(created\.token\)/);
  assert.match(managementPages, /api<\{ token: string \}>\(`\/internal\/v1\/service-tokens\/\$\{value\.service_id\}\/rotate`/);
  assert.match(managementPages, /showSecret\(result\.token\)/);
  assert.match(portal, /<CopyButton value=\{credentialInput\}/);
  assert.match(portal, /<CopyButton value=\{credential\}/);
  assert.match(settings, /<CopyButton value=\{credentialInput\}/);
  assert.match(settings, /<CopyButton value=\{credential\}/);
  assert.doesNotMatch(portal, /<CopyButton value=\{credentialView\.key_id\}/);
  assert.doesNotMatch(managementPages, /<OneTimeSecret value=\{value\.key_id\}/);
  assert.doesNotMatch(managementPages, /<OneTimeSecret value=\{value\.service_id\}/);
});

test('service credentials cannot be double-issued or overwrite visible plaintext', () => {
  assert.match(serviceWorkspace, /Symbol\('service-credential-secret-operation'\)/);
  assert.match(serviceWorkspace, /if \(secretOperation\.current \|\| secretRef\.current\) return undefined/);
  assert.match(serviceWorkspace, /secretRef\.current = next;\s*setSecret\(next\)/);
  assert.match(serviceWorkspace, /secret\?\.scopeGeneration === renderScope\.current\.generation/);
  assert.match(serviceWorkspace, /renderScope\.current\.generation === operationScopeGeneration/);
  assert.match(serviceWorkspace, /filename="service-credential\.txt" onDismiss=\{dismissSecret\}/);
  assert.match(serviceWorkspace, /onClick=\{\(\) => void rotateServiceCredential\(value\)\}/);
  assert.match(serviceWorkspace, /onSubmit=\{\(\{ formData \}\) => \{ void createServiceCredential\(formData\); \}\}/);
  assert.match(serviceWorkspace, /disabled=\{!canManage\(value\) \|\| value\.status === 'revoked' \|\| Boolean\(busy\) \|\| Boolean\(visibleSecret\)\}/);
  assert.match(serviceWorkspace, /disabled=\{!writeTenant \|\| Boolean\(busy\) \|\| Boolean\(visibleSecret\)\}/);
  assert.doesNotMatch(serviceWorkspace, /setSecret\((created|result)\.token\)/);
});
