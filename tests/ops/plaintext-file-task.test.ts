import assert from 'node:assert/strict';
import { test } from 'node:test';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { chatTurn, fileTool, runTask } from '../../scripts/maintenance/plaintext-file-task.ts';

const sse = (chunks: unknown[], terminal = true) => new Response(chunks.map(c => `data: ${JSON.stringify(c)}\r\n\r\n`).join('') + (terminal ? 'data: [DONE]\r\n\r\n' : ''), { headers: { 'content-type': 'text/event-stream', 'x-mtc-request-id': 'actual-request-id' } });

test('complete streamed tool loop preserves arguments, IDs and provider reasoning replay without exposing reasoning', async () => {
  const directory = await fs.mkdtemp(path.join(os.tmpdir(), 'mtc-file-task-'));
  const archive = directory + '-receipt.json';
  const payloads: any[] = [];
  try {
    const argumentsText = JSON.stringify({ path: 'result.js', content: 'export const answer = 42;\n' });
    const result = await runTask({ directory, archive, task: 'Create result.js.', model: 'test-model', apiKey: 'private-key', baseUrl: 'https://example.invalid/v1' }, async (_url, request) => {
      payloads.push(JSON.parse(request!.body as string));
      if (payloads.length === 1) return sse([
        { choices: [{ delta: { reasoning_content: 'private reasoning', tool_calls: [{ index: 0, id: 'call_1', type: 'function', function: { name: 'write_file', arguments: argumentsText.slice(0, 17) } }] } }] },
        { choices: [{ delta: { tool_calls: [{ index: 0, function: { arguments: argumentsText.slice(17) } }] }, finish_reason: 'tool_calls' }] },
      ]);
      return sse([{ model: 'actual-model', choices: [{ delta: { content: 'Created result.js.' }, finish_reason: 'stop' }], usage: { total_tokens: 12 } }]);
    });
    assert.equal(payloads.length, 2);
    assert.equal(payloads[1].messages[2].tool_calls[0].function.arguments, argumentsText);
    assert.equal(payloads[1].messages[2].reasoning_content, 'private reasoning');
    assert.equal(payloads[1].messages[3].tool_call_id, 'call_1');
    assert.equal(JSON.parse(payloads[1].messages[3].content).ok, true);
    assert.equal(await fs.readFile(path.join(directory, 'result.js'), 'utf8'), 'export const answer = 42;\n');
    assert.equal(result.completed, true);
    assert.equal(result.turns[1].request_id, 'actual-request-id');
    const receipt = await fs.readFile(archive, 'utf8');
    assert(!receipt.includes('private reasoning'));
    assert(!receipt.includes('private-key'));
    assert(receipt.includes('Create result.js.'));
  } finally { await fs.rm(directory, { recursive: true, force: true }); await fs.rm(directory + '-receipt.json', { force: true }); }
});

test('missing DONE never executes a partially received write and never retries', async () => {
  const directory = await fs.mkdtemp(path.join(os.tmpdir(), 'mtc-file-task-'));
  let requests = 0;
  try {
    const result = await runTask({ directory, archive: directory + '-receipt.json', task: 'Task', model: 'test', apiKey: 'private-key', baseUrl: 'https://example.invalid' }, async () => {
      requests++;
      return sse([{ choices: [{ delta: { tool_calls: [{ index: 0, id: 'call_1', function: { name: 'write_file', arguments: '{"path":"bad","content":"bad"}' } }] }, finish_reason: 'tool_calls' }] }], false);
    });
    assert.equal(result.completed, false);
    assert.equal(requests, 1);
    await assert.rejects(fs.stat(path.join(directory, 'bad')));
  } finally { await fs.rm(directory, { recursive: true, force: true }); await fs.rm(directory + '-receipt.json', { force: true }); }
});

test('file operations stay in isolated directory, including symlink destinations', async () => {
  const directory = await fs.mkdtemp(path.join(os.tmpdir(), 'mtc-file-task-'));
  const outside = await fs.mkdtemp(path.join(os.tmpdir(), 'mtc-file-task-outside-'));
  try {
    await fs.writeFile(path.join(outside, 'original'), 'preserved');
    await fs.symlink(path.join(outside, 'original'), path.join(directory, 'link'));
    await assert.rejects(fileTool(directory, 'write_file', { path: '../outside', content: 'bad' }));
    await assert.rejects(fileTool(directory, 'write_file', { path: 'link', content: 'bad' }));
    await assert.rejects(fileTool(directory, 'read_file', { path: 'link' }));
    assert.equal(await fs.readFile(path.join(outside, 'original'), 'utf8'), 'preserved');
  } finally { await fs.rm(directory, { recursive: true, force: true }); await fs.rm(outside, { recursive: true, force: true }); }
});

test('duplicate tool identity is incomplete and non-JSON upstream error remains an actual failure', async () => {
  const duplicate = await chatTurn(sse([{ choices: [{ delta: { tool_calls: [0, 1].map(index => ({ index, id: 'same', function: { name: 'read_file', arguments: '{"path":"a"}' } })) }, finish_reason: 'tool_calls' }] }]));
  assert.equal(duplicate.stream_complete, false);
  const error = await chatTurn(new Response('upstream unavailable', { status: 503 }));
  assert.equal(error.status, 503);
  assert.equal(error.stream_complete, false);
});
