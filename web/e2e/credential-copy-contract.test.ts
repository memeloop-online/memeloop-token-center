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

test('credential copying shares clipboard handling with accessible failure feedback', () => {
  assert.match(copyButton, /navigator\.clipboard\?\.writeText/);
  assert.match(copyButton, /navigator\.clipboard\.writeText\(value\)/);
  assert.match(copyButton, /catch \{\s*setState\('failed'\);/s);
  assert.match(copyButton, /role="status" aria-live="polite"/);
  assert.match(copyButton, /common\.requestFailed/);
  assert.match(oneTimeSecret, /<CopyButton value=\{value\}/);
});

test('only current plaintext credential sources are offered for copying', () => {
  assert.match(managementPages, /<OneTimeSecret value=\{secret\}/);
  assert.match(managementPages, /api<\{ key: string; key_id: string \}>\('\/internal\/v1\/keys'/);
  assert.match(managementPages, /setSecret\(created\.key\)/);
  assert.match(managementPages, /api<\{ key: string \}>\(`\/internal\/v1\/keys\/\$\{value\.key_id\}\/rotate`/);
  assert.match(managementPages, /setSecret\(result\.key\)/);
  assert.match(managementPages, /api<\{ token: string \}>\('\/internal\/v1\/service-tokens'/);
  assert.match(managementPages, /setSecret\(created\.token\)/);
  assert.match(managementPages, /api<\{ token: string \}>\(`\/internal\/v1\/service-tokens\/\$\{value\.service_id\}\/rotate`/);
  assert.match(managementPages, /setSecret\(result\.token\)/);
  assert.match(portal, /<CopyButton value=\{credentialInput\}/);
  assert.match(portal, /<CopyButton value=\{credential\}/);
  assert.match(settings, /<CopyButton value=\{credentialInput\}/);
  assert.match(settings, /<CopyButton value=\{credential\}/);
  assert.doesNotMatch(portal, /<CopyButton value=\{credentialView\.key_id\}/);
  assert.doesNotMatch(managementPages, /<OneTimeSecret value=\{value\.key_id\}/);
  assert.doesNotMatch(managementPages, /<OneTimeSecret value=\{value\.service_id\}/);
});
