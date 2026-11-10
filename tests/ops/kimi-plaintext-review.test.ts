import assert from 'node:assert/strict';
import test from 'node:test';
import { review } from '../../scripts/maintenance/kimi-review.ts';

test('plaintext review sends the complete task once without changing provider thinking or output limits', async () => {
  const task = 'Review this complete patch.\n\n中文任务\n+ actual source change\n';
  let calls = 0;
  const result = await review({ baseUrl: 'https://gateway.invalid/v1/', apiKey: 'test-key', model: 'k3.1-pro', task }, async (url, init) => {
    calls++;
    assert.equal(url, 'https://gateway.invalid/v1/chat/completions');
    assert.equal((init?.headers as Record<string, string>).Authorization, 'Bearer test-key');
    assert.deepEqual(JSON.parse(init?.body as string), {
      model: 'k3.1-pro', messages: [{ role: 'user', content: task }], stream: false,
    });
    return new Response(JSON.stringify({ model: 'k3.1-pro', choices: [{ finish_reason: 'stop', message: { content: 'Review findings.' } }], usage: { completion_tokens: 12000 } }), { headers: { 'x-request-id': 'review-request' } });
  });
  assert.equal(calls, 1);
  assert.equal(result.request_id, 'review-request');
  assert.equal(result.review, 'Review findings.');
  assert.equal(result.finish_reason, 'stop');
  assert.deepEqual(result.usage, { completion_tokens: 12000 });
});

test('provider failure is returned without a retry or exposing a credential in its message', async () => {
  let calls = 0;
  const result = await review({ baseUrl: 'https://gateway.invalid/v1', apiKey: 'test-key', model: 'k3.1-pro', task: 'Review.' }, async () => {
    calls++;
    return new Response(JSON.stringify({ error: { type: 'upstream_error', message: 'rejected test-key' } }), { status: 400 });
  });
  assert.equal(calls, 1);
  assert.equal(result.status, 400);
  assert.equal(result.review, undefined);
  assert.equal(result.error?.message, 'rejected [redacted]');
});
