import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const read = (path: string) => readFile(new URL(path, import.meta.url), 'utf8');
test('proxy update remains a redacted, explicit credential-versioned contract', async () => {
  const source = await read('../src/operator/UpstreamConnection.tsx');
  assert.match(source, /type="password" required aria-invalid=\{invalid\} aria-describedby=/);
  assert.match(source, /expected_credential_generation: account.credential_generation/);
  assert.match(source, /'Idempotency-Key': crypto.randomUUID\(\)/);
  assert.match(source, /account.has_proxy === undefined \? 'connection.proxyUnknown'/);
  assert.match(source, /account.can_update_transport_proxy !== false/);
  assert.doesNotMatch(source, /localStorage|sessionStorage|console\./);
});
test('model submission validates integer priority and effective upstream protocol compatibility', async () => {
  const source = await read('../src/operator/pages/ManagementPages.tsx');
  assert.match(source, /Number.isInteger\(draft.priority\) && Math.abs\(draft.priority\) <= 1000000/);
  assert.match(source, /catalogValid && compatible && candidates.length > 0/);
  assert.match(source, /provider.protocols.includes\(draft.protocol\)/);
  for (const section of ['identitySection', 'upstreamSection', 'accessSection']) assert.ok(source.includes(`<legend>{t('routes.${section}')}</legend>`));
});
test('quota consume remains behind prepare, contract validation and explicit confirmation', async () => {
  const source = await read('../src/operator/UpstreamQuotaReset.tsx');
  assert.ok(source.indexOf('const accepted = await confirm(') < source.indexOf("confirmation: 'consume_one_supplier_reset_credit'"));
  assert.match(source, /if \(!accepted \|\| !confirmationToken \|\| result.operation.expires_at <= Date.now\(\)\)/);
  assert.match(source, /'Idempotency-Key': confirmKey.current/);
  assert.match(source, /response.id !== operation.id/);
  const browser = await read('./upstream-connection-browser.test.ts');
  assert.doesNotMatch(browser, /(?:quota-heading|quota-reset-action|button.danger).*\.click\(\)/);
});
