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
      model: 'k3.1-pro', messages: [{ role: 'user', content: task }], stream: true, stream_options: { include_usage: true },
    });
    return new Response(JSON.stringify({ model: 'k3.1-pro', choices: [{ finish_reason: 'stop', message: { content: 'Review findings.' } }], usage: { completion_tokens: 12000 } }), { headers: { 'x-request-id': 'review-request' } });
  });
  assert.equal(calls, 1);
  assert.equal(result.request_id, 'review-request');
  assert.equal(result.review, 'Review findings.');
  assert.equal(result.finish_reason, 'stop');
  assert.deepEqual(result.usage, { completion_tokens: 12000 });
});

test('streaming review joins split UTF-8 content and terminal usage without returning reasoning', async () => {
  const frames = [
    { model: 'k3.1-pro', choices: [{ delta: { reasoning_content: 'private reasoning', content: '中文' } }] },
    { choices: [{ delta: { content: '审查完成' }, finish_reason: 'stop' }] },
    { choices: [], usage: { completion_tokens: 12000 } },
  ].map(chunk => `data: ${JSON.stringify(chunk)}\r\n\r\n`).join('') + 'data: [DONE]\r\n\r\n';
  const bytes = new TextEncoder().encode(frames);
  let position = 0;
  const body = new ReadableStream<Uint8Array>({ pull(controller) {
    if (position === bytes.length) return controller.close();
    controller.enqueue(bytes.slice(position, position + 1));
    position++;
  } });
  const result = await review({ baseUrl: 'https://gateway.invalid/v1', apiKey: 'test-key', model: 'k3.1-pro', task: 'Review.' }, async () => new Response(body, { headers: { 'content-type': 'text/event-stream' } }));
  assert.equal(result.review, '中文审查完成');
  assert.equal(result.finish_reason, 'stop');
  assert.equal(result.stream_complete, true);
  assert.deepEqual(result.usage, { completion_tokens: 12000 });
  assert.ok(!JSON.stringify(result).includes('private reasoning'));
});

test('an interrupted stream returns its partial text with an explicit incomplete result', async () => {
  const body = 'data: {"choices":[{"delta":{"content":"Partial review"}}]}\n\n';
  const result = await review({ baseUrl: 'https://gateway.invalid/v1', apiKey: 'test-key', model: 'k3.1-pro', task: 'Review.' }, async () => new Response(body, { headers: { 'content-type': 'text/event-stream' } }));
  assert.equal(result.review, 'Partial review');
  assert.equal(result.stream_complete, false);
  assert.equal(result.finish_reason, undefined);
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

 test('a non-JSON gateway failure preserves status and request ID without retry', async () => {
  const result = await review({ baseUrl: 'https://gateway.invalid/v1', apiKey: 'test-key', model: 'k3.1-pro', task: 'Review.' }, async () => new Response('upstream reset test-key', { status: 503, headers: { 'x-request-id': 'reset-request' } }));
  assert.equal(result.status, 503);
  assert.equal(result.request_id, 'reset-request');
  assert.deepEqual(result.error, { type: 'non_json_response', message: 'upstream reset [redacted]', code: undefined });
});
