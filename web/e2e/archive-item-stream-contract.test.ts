import assert from 'node:assert/strict';
import test from 'node:test';
import { ArchiveItemStream, type ArchiveStreamItem } from '../src/archiveItemStream.js';

function read(text: string, side: 'request' | 'response', bytePage: number, itemPage = 1) {
  const bytes = new TextEncoder().encode(text);
  const decoder = new TextDecoder('utf-8', { fatal: true });
  const parser = new ArchiveItemStream(side);
  const result: ArchiveStreamItem[] = [];
  for (let offset = 0; offset < bytes.length; offset += bytePage) {
    parser.append(decoder.decode(bytes.slice(offset, offset + bytePage), { stream: offset + bytePage < bytes.length }));
    let items = parser.take(itemPage);
    while (items.length) { result.push(...items); items = parser.take(itemPage); }
  }
  parser.finish();
  return result;
}

test('incremental JSON preserves Unicode, escaped structure, and item boundaries across byte pages', () => {
  const input = [{ role: 'user', content: '你好🌱\\\"[]{}' }, { type: 'custom_tool_call', input: 'x'.repeat(20_000) }];
  const archive = JSON.stringify({ instructions: 'ignored'.repeat(20_000), input, metadata: { nested: true } });
  for (const page of [1, 2, 3, 65_536]) {
    assert.deepEqual(read(archive, 'request', page), input.map(value => ({ format: 'input', value })));
  }
});

test('large archives have no total item or byte cutoff', () => {
  const output = Array.from({ length: 1_200 }, (_, index) => ({ type: 'message', role: 'assistant', content: [{ type: 'output_text', text: `${index}:${'a'.repeat(1_000)}` }] }));
  assert.deepEqual(read(JSON.stringify({ output }), 'response', 65_536, 30), output.map(value => ({ format: 'output', value })));
});

test('wrapped response scalar, empty supported arrays, and bare text retain their actual shape', () => {
  assert.deepEqual(read('{"response":{"output_text":"answer"}}', 'response', 2), [{ format: 'output_text', value: 'answer' }]);
  assert.deepEqual(read('{"messages":[]}', 'request', 1), []);
  assert.deepEqual(read('"hello"', 'request', 1), [{ format: 'input', value: 'hello' }]);
});

test('unsupported, truncated, and malformed envelopes do not become a successful empty archive', () => {
  for (const archive of ['{"error":{"message":"unavailable"}}', '{"input":"wrong side"}', '{"output":[', '{"output":[]}garbage', '{"output":[] "other":1}', '{"output":[],}', '{"output":[],"ignored":"bad\\q"}']) {
    assert.throws(() => read(archive, 'response', 1), archive);
  }
});

test('SSE emits actual completed output items in output_index order, without duplicate terminal output', () => {
  const first = { type: 'message', role: 'assistant', content: [{ type: 'output_text', text: 'first' }] };
  const second = { type: 'custom_tool_call', call_id: 'tool', input: 'second' };
  const frame = (event: unknown) => `data: ${JSON.stringify(event)}\r\n\r\n`;
  const archive = 'id: archive\r\nretry: 1000\r\n\r\n'
    + frame({ type: 'response.output_item.done', output_index: 1, item: second })
    + frame({ type: 'response.output_item.done', output_index: 0, item: first })
    + frame({ type: 'response.completed', response: { output: [first, second] } });
  assert.deepEqual(read(archive, 'response', 3), [first, second].map(value => ({ format: 'output', value })));
  const terminalOnly = frame({ type: 'response.output_item.done', output_index: 1, item: second })
    + frame({ type: 'response.completed', response: { output: [first, second] } });
  assert.deepEqual(read(terminalOnly, 'response', 2), [first, second].map(value => ({ format: 'output', value })));
  assert.throws(() => read(frame({ type: 'response.output_item.done', output_index: 0, item: first }), 'response', 3), /terminal/);
  assert.throws(() => read(frame({ type: 'response.incomplete', response: { output: [first] } }), 'response', 3), /did not complete/);
});
