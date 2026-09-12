import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import { connectionSchema, isPrivateProxyUrl } from '../src/operator/upstreamConnectionPolicy.js';

test('Codex proxies mirror private backend ranges without narrowing generic hostname schemas', () => {
  for (const [url, valid] of [
    ['socks5h://10.0.0.10:1080', true], ['socks5h://100.64.0.16', true], ['socks5h://[fd00::1]:1080', true],
    ['socks5h://100.100.100.200:1080', false], ['socks5h://127.0.0.1:1080', false], ['socks5h://8.8.8.8:1080', false],
    ['socks5://10.0.0.10:1080', false], ['socks5h://proxy.example:1080', false], ['socks5h://10.0.0.10:0', false],
  ] as const) assert.equal(isPrivateProxyUrl(url), valid, url);
  const generic = { type: 'object' as const, properties: { base_url: { type: 'string' as const }, proxy_url: { type: 'string' as const, pattern: '^socks5h?://' } } };
  const output = connectionSchema(generic, 'API endpoint, not a proxy');
  assert.deepEqual(output.properties?.proxy_url, generic.properties.proxy_url);
  assert.equal((output.properties?.base_url as { readOnly?: boolean }).readOnly, undefined);
  const fixed = connectionSchema({ properties: { base_url: { const: 'https://chatgpt.com/backend-api/codex' } } }, 'Fixed endpoint');
  assert.equal((fixed.properties?.base_url as { readOnly?: boolean }).readOnly, true);
});

test('proxy writes stay versioned, capability-gated and secret-free; model IA preserves integer validation', async () => {
  const source = await readFile(new URL('../src/operator/UpstreamConnection.tsx', import.meta.url), 'utf8');
  assert.match(source, /account.can_update_transport_proxy === true/);
  assert.match(source, /expected_credential_generation: account.credential_generation/);
  assert.match(source, /'Idempotency-Key': crypto.randomUUID\(\)/);
  assert.doesNotMatch(source, /localStorage|sessionStorage|console\./);
  const forms = await readFile(new URL('../src/operator/pages/ManagementPages.tsx', import.meta.url), 'utf8');
  assert.match(forms, /Number.isInteger\(draft.priority\) && Math.abs\(draft.priority\) <= 1000000/);
  for (const section of ['identitySection', 'upstreamSection', 'accessSection']) assert.ok(forms.includes(`<legend>{t('routes.${section}')}</legend>`));
});
