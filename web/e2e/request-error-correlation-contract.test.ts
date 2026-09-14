import assert from 'node:assert/strict';
import test from 'node:test';
import { api, apiDiagnosticMessage, ApiError, streamSse } from '../src/api.js';

const requestId = '01900000-0000-7000-8000-000000000001';

test('JSON and plain ingress errors retain HTTP status and only a validated service ID', async () => {
  const original = globalThis.fetch;
  try {
    for (const json of [true, false]) {
      globalThis.fetch = async () => new Response(json ? JSON.stringify({ error: { message: 'Unavailable', code: 'overloaded' } }) : 'secret-body-canary', {
        status: 503, headers: { 'x-mtc-request-id': requestId },
      });
      await assert.rejects(api('/internal/v1/requests/query', 'credential-canary'), (error: unknown) => {
        assert.ok(error instanceof ApiError);
        assert.equal(error.requestId, requestId);
        const message = apiDiagnosticMessage(error, 'fallback');
        assert.match(message, /HTTP 503/);
        assert.ok(message.includes(requestId));
        assert.doesNotMatch(message, /secret-body-canary|credential-canary/);
        return true;
      });
    }
    globalThis.fetch = async () => new Response('', { status: 502, headers: { 'x-mtc-request-id': 'secret-header-canary' } });
    await assert.rejects(api('/internal/v1/upstreams', 'credential-canary'), (error: unknown) => {
      assert.ok(error instanceof ApiError);
      assert.equal(error.requestId, undefined);
      return true;
    });
  } finally { globalThis.fetch = original; }
});

test('SSE failures distinguish rejected headers from a failed 200 body without exposing contents', async () => {
  const original = globalThis.fetch;
  try {
    for (const status of [503, 200]) {
      globalThis.fetch = async () => new Response(status === 200 ? new ReadableStream({
        start(controller) { controller.error(new Error('secret-stream-canary')); },
      }) : '', { status, headers: { 'x-mtc-request-id': requestId } });
      await assert.rejects(streamSse('/internal/v1/request-events', 'credential-canary', new AbortController().signal, () => {}), (error: unknown) => {
        assert.ok(error instanceof ApiError);
        assert.equal(error.status, status);
        assert.equal(error.requestId, requestId);
        assert.doesNotMatch(apiDiagnosticMessage(error, 'fallback'), /secret-stream-canary|credential-canary/);
        return true;
      });
    }
  } finally { globalThis.fetch = original; }
});
