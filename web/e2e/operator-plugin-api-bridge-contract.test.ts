import assert from 'node:assert/strict';
import test from 'node:test';
import { createOperatorApiBridge } from '../src/plugins/operatorApiBridge.js';

test('plugin bridge pins authentication, tenant, transport and abort signal', async () => {
  const originalFetch = globalThis.fetch;
  const controller = new AbortController();
  let observed: { path: string; init: RequestInit } | undefined;
  globalThis.fetch = async (path, init) => {
    if (String(path) === '/internal/v1/api-schema') return new Response(JSON.stringify({ paths: {
      '/internal/v1/resources': { post: { requestBody: { content: { 'application/json': { schema: { properties: { tenant_external_id: { type: 'string' } } } } } } } },
    } }));
    observed = { path: String(path), init: init! };
    return new Response(JSON.stringify({ ok: true }), { status: 200 });
  };
  try {
    const result = await createOperatorApiBridge('host-secret', 'active-tenant')({
      path: '/internal/v1/resources?%74enant_external_id=other&tenant_external_id=third&limit=2',
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
  // A tenant-bearing body is rejected before issuing any request.
  await assert.rejects(request({ path: '/v1/resources', method: 'POST', body: { tenant_external_id: 'other' } }), /active tenant/);
});


test('plugin bridge injects omitted JSON tenants using cached discovery, preserving global bodies', async () => {
  const originalFetch = globalThis.fetch;
  let discoveries = 0;
  const sent: unknown[] = [];
  globalThis.fetch = async (path, init) => {
    if (String(path) === '/internal/v1/api-schema') {
      discoveries += 1;
      return new Response(JSON.stringify({
        paths: {
          '/internal/v1/keys': { post: { requestBody: { content: { 'application/json': { schema: { $ref: '#/components/schemas/Create' } } } } } },
          '/internal/v1/global/{id}': { put: { requestBody: { content: { 'application/json': { schema: { type: 'object', properties: { value: { type: 'number' } }, additionalProperties: false } } } } } },
        },
        components: { schemas: { Create: { allOf: [{ properties: { tenant_external_id: { type: 'string', default: 'default' } } }] } } },
      }));
    }
    sent.push(JSON.parse(String(init!.body)));
    return new Response('{}');
  };
  try {
    const request = createOperatorApiBridge('credential', 'selected');
    await request({ path: '/internal/v1/keys?%74enant_external_id=other&tenant_external_id=third', method: 'POST', body: { alias: 'created' } });
    await request({ path: '/internal/v1/global/id', method: 'PUT', body: { value: 3 } });
    assert.deepEqual(sent, [{ alias: 'created', tenant_external_id: 'selected' }, { value: 3 }]);
    assert.equal(discoveries, 1);
    await assert.rejects(request({ path: '/internal/v1/undocumented', method: 'POST', body: {} }), /absent from/);
    assert.equal(sent.length, 2);
    await assert.rejects(createOperatorApiBridge('credential', '')({ path: '/internal/v1/keys', method: 'POST', body: { alias: 'created' } }), /require an active tenant/);
    assert.equal(sent.length, 2);
  } finally { globalThis.fetch = originalFetch; }
});

test('aborted discovery does not poison subsequent writes', async () => {
  const originalFetch = globalThis.fetch;
  let discoveries = 0;
  globalThis.fetch = async (path, init) => {
    init?.signal?.throwIfAborted();
    if (String(path) === '/internal/v1/api-schema') {
      discoveries += 1;
      return new Response(JSON.stringify({ paths: { '/internal/v1/global': { post: {} } } }));
    }
    return new Response('{}');
  };
  try {
    const request = createOperatorApiBridge('credential', 'selected');
    const controller = new AbortController();
    controller.abort();
    await assert.rejects(request({ path: '/internal/v1/global', method: 'POST', body: {}, signal: controller.signal }));
    await request({ path: '/internal/v1/global', method: 'POST', body: {} });
    assert.equal(discoveries, 1);
  } finally { globalThis.fetch = originalFetch; }
});

test('tenant detection prefers specific paths and rejects equal-rank templates before dispatch', async () => {
  const originalFetch = globalThis.fetch;
  const sent: unknown[] = [];
  const tenantWrite = { post: { requestBody: { content: { 'application/json': { schema: {
    properties: { tenant_external_id: { type: 'string' } },
  } } } } } };
  globalThis.fetch = async (path, init) => {
    if (String(path) === '/internal/v1/api-schema') return new Response(JSON.stringify({ paths: {
      '/internal/v1/{resource}/{id}': tenantWrite,
      '/internal/v1/keys/{id}': { post: {} },
      '/internal/v1/keys/rotate': tenantWrite,
      '/internal/v1/{resource}/rotate': tenantWrite,
    } }));
    sent.push(JSON.parse(String(init!.body)));
    return new Response('{}');
  };
  try {
    const request = createOperatorApiBridge('credential', 'selected');
    await request({ path: '/internal/v1/keys/plain', method: 'POST', body: { value: 1 } });
    await request({ path: '/internal/v1/keys/rotate', method: 'POST', body: { value: 2 } });
    assert.deepEqual(sent, [{ value: 1 }, { value: 2, tenant_external_id: 'selected' }]);
    // Both templates have the same literal count; neither is authoritative.
    globalThis.fetch = async (path) => {
      assert.equal(String(path), '/internal/v1/api-schema');
      return new Response(JSON.stringify({ paths: {
        '/internal/v1/keys/{id}': { post: {} },
        '/internal/v1/{resource}/rotate': tenantWrite,
      } }));
    };
    await assert.rejects(createOperatorApiBridge('credential', 'selected')({
      path: '/internal/v1/keys/rotate', method: 'POST', body: {},
    }), /path is ambiguous/);
  } finally { globalThis.fetch = originalFetch; }
});
