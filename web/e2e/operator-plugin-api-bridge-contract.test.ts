import assert from 'node:assert/strict';
import test from 'node:test';
import { createOperatorApiBridge } from '../src/plugins/operatorApiBridge.js';

test('plugin bridge pins authentication, tenant, transport and abort signal', async () => {
  const originalFetch = globalThis.fetch;
  const controller = new AbortController();
  let observed: { path: string; init: RequestInit } | undefined;
  globalThis.fetch = async (path, init) => {
    observed = { path: String(path), init: init! };
    return new Response(JSON.stringify({ ok: true }), { status: 200 });
  };
  try {
    const result = await createOperatorApiBridge('host-secret', 'active-tenant')({
      path: '/internal/v1/resources?tenant_external_id=other&limit=2',
      method: 'POST', body: { tenant_external_id: 'other', value: 3 }, signal: controller.signal,
    });
    assert.deepEqual(result, { ok: true });
    assert.equal(observed!.path, '/internal/v1/resources?tenant_external_id=active-tenant&limit=2');
    assert.equal((observed!.init.headers as Record<string, string>).Authorization, 'Bearer host-secret');
    assert.deepEqual(JSON.parse(String(observed!.init.body)), { tenant_external_id: 'active-tenant', value: 3 });
    assert.equal(observed!.init.signal, controller.signal);
    assert.equal(observed!.init.redirect, 'error');
    assert.equal(observed!.init.credentials, 'same-origin');
  } finally { globalThis.fetch = originalFetch; }
});

test('plugin bridge rejects URL escapes before sending the host credential', async () => {
  const request = createOperatorApiBridge('host-secret', 'tenant');
  for (const path of [
    'https://evil.invalid/v1/models', '//evil.invalid/v1/models', '/v1/../logout',
    '/v1/%2e%2e/logout', '/v1/\\evil.invalid', '/v1/models#fragment',
    '/logout', '/v1/models\n', '/internal/v1/../../logout',
  ]) await assert.rejects(request({ path }), /API path/);
  await assert.rejects(request({ path: '/v1/models', method: 'GET', body: {} }), /cannot include a body/);
});

test('plugin bridge preserves server RBAC failures and cancellation', async () => {
  const originalFetch = globalThis.fetch;
  try {
    globalThis.fetch = async () => new Response(JSON.stringify({ error: { message: 'Forbidden', code: 'forbidden' } }), { status: 403 });
    await assert.rejects(createOperatorApiBridge('credential', 'tenant')({ path: '/internal/v1/resources' }),
      (error: unknown) => error instanceof Error && 'status' in error && error.status === 403);
    const controller = new AbortController();
    controller.abort();
    globalThis.fetch = async (_path, init) => { init!.signal!.throwIfAborted(); throw new Error('unexpected'); };
    await assert.rejects(createOperatorApiBridge('credential', 'tenant')({ path: '/internal/v1/resources', signal: controller.signal }),
      (error: unknown) => error instanceof DOMException && error.name === 'AbortError');
  } finally { globalThis.fetch = originalFetch; }
});

test('plugin bridge cannot select a tenant while host scope is empty', async () => {
  const request = createOperatorApiBridge('credential', '');
  await assert.rejects(request({ path: '/internal/v1/resources?tenant_external_id=other' }), /active tenant/);
  await assert.rejects(request({ path: '/internal/v1/resources', method: 'POST', body: { tenant_external_id: 'other' } }), /active tenant/);
});
