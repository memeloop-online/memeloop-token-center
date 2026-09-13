import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const [copyButton, managementPages] = await Promise.all([
  readFile(new URL('../src/CopyButton.tsx', import.meta.url), 'utf8'),
  readFile(new URL('../src/operator/pages/ManagementPages.tsx', import.meta.url), 'utf8'),
]);

test('generic credential copying exposes accessible failure feedback', () => {
  assert.match(copyButton, /navigator\.clipboard\.writeText\(value\)/);
  assert.match(copyButton, /catch \{\s*setState\('failed'\);/s);
  assert.match(copyButton, /role="status" aria-live="polite"/);
});

test('operator credential panels bind only secret response fields to plaintext', () => {
  assert.match(managementPages, /<OneTimeSecret key=\{visibleSecret\.displayId\} value=\{visibleSecret\.value\}/);
  assert.match(managementPages, /api<\{ key: string; key_id: string \}>\('\/internal\/v1\/keys'/);
  assert.match(managementPages, /api<\{ token: string \}>\('\/internal\/v1\/service-tokens'/);
  assert.doesNotMatch(managementPages, /<OneTimeSecret value=\{value\.key_id\}/);
  assert.doesNotMatch(managementPages, /<OneTimeSecret value=\{value\.service_id\}/);
});
