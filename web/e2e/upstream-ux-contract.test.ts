import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import { connectionSchema, isPrivateProxyUrl } from '../src/operator/upstreamConnectionPolicy.js';

test('versioned failover fields retain schema bounds and receive editor labels', () => {
  const fields = {
    version: { type: 'integer' as const, enum: [1], default: 1 },
    candidate_attempts: { type: 'integer' as const, minimum: 1, maximum: 8, default: 3 },
    failover_deadline_millis: { type: 'integer' as const, minimum: 1000, maximum: 300000, default: 300000 },
  };
  const output = connectionSchema({ properties: { transport_policy: { properties: fields, additionalProperties: false } } }, 'Fixed endpoint');
  const policy = output.properties?.transport_policy as { properties: Record<string, object>; additionalProperties: boolean };
  assert.equal(policy.additionalProperties, false);
  for (const [field, title] of Object.entries({ version: 'Policy version', candidate_attempts: 'Candidate attempts', failover_deadline_millis: 'Failover deadline (ms)' })) {
    assert.deepEqual(policy.properties[field], { ...fields[field as keyof typeof fields], title });
  }
  assert.equal('title' in fields.version, false);
});

test('SSE framing policy fields stay in the runtime connection policy editor', () => {
  const fields = {
    max_sse_event_bytes: { type: 'integer' as const, minimum: 262144, maximum: 16777216, default: 8388608 },
    max_sse_framed_bytes: { type: 'integer' as const, minimum: 262144, maximum: 16842752, default: 8454144 },
    max_sse_terminal_hold_bytes: { type: 'integer' as const, minimum: 262144, maximum: 16842752, default: 8454144 },
  };
  const output = connectionSchema({ properties: { transport_policy: { properties: fields, additionalProperties: false } } }, 'Fixed endpoint');
  const policy = output.properties?.transport_policy as { title?: string; properties: Record<string, { title?: string }> };
  assert.equal(policy.title, 'Runtime transport policy');
  assert.equal(policy.properties.max_sse_event_bytes.title, 'Maximum SSE event (bytes)');
  assert.equal(policy.properties.max_sse_framed_bytes.title, 'Maximum framed chunk (bytes)');
  assert.equal(policy.properties.max_sse_terminal_hold_bytes.title, 'Maximum terminal hold (bytes)');
});

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
  for (const section of ['identitySection', 'upstreamSection']) assert.ok(forms.includes(`<FormSection title={t('routes.${section}')}`));
  assert.ok(forms.includes('<FormSection title={journey.routeAccess}'));
  const sections = await readFile(new URL('../src/design-system/primitives.tsx', import.meta.url), 'utf8');
  assert.match(sections, /<fieldset className="mtc-form-section"/);
  assert.match(sections, /<legend>\{title\}<\/legend>/);
});
