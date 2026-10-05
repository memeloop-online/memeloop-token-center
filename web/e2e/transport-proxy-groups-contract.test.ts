import assert from 'node:assert/strict';
import test from 'node:test';
import { ApiError } from '../src/api.js';
import { transportProxyError, transportProxyFailureKind, transportProxyRequest } from '../src/operator/transportProxyGroups.js';

test('transport proxy failures distinguish editable validation, CAS, denied and uncertain outcomes without exposing response secrets', () => {
  const secret = 'socks5h://privateProxySecret@10.0.0.1:1080';
  for (const [status, code, expected] of [
    [400, 'invalid_request', 'validation'], [422, undefined, 'validation'],
    [409, 'proxy_group_version_conflict', 'conflict'], [409, 'proxy_group_binding_conflict', 'conflict'],
    [403, 'forbidden', 'denied'], [503, 'service_overloaded', 'unknown'], [200, undefined, 'unknown'],
  ] as const) {
    const reason = new ApiError(secret, status, code);
    assert.equal(transportProxyFailureKind(reason), expected);
    assert.ok(!transportProxyError(reason).includes(secret));
  }
  assert.equal(transportProxyFailureKind(new TypeError(secret)), 'unknown');
  assert.match(transportProxyError(new TypeError(secret)), /不表示服务端已取消/);
});

test('stopping client wait rejects even when fetch ignores abort; late response does not turn the result into success', async () => {
  const original = globalThis.fetch;
  let release!: (response: Response) => void;
  let requestSignal: AbortSignal | null | undefined;
  globalThis.fetch = async (_input, init) => {
    requestSignal = init?.signal;
    return new Promise<Response>(resolve => { release = resolve; });
  };
  try {
    const controller = new AbortController();
    const pending = transportProxyRequest('/fixture', 'fixture-only', { method: 'PUT', body: '{}', signal: controller.signal });
    controller.abort();
    await assert.rejects(pending, /result unknown/);
    assert.equal(requestSignal?.aborted, true);
    release(new Response('{}'));
    await assert.rejects(pending, /result unknown/);
  } finally { globalThis.fetch = original; }
});
