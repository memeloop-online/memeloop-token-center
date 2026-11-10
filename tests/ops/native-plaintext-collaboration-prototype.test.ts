import assert from 'node:assert/strict';
import test from 'node:test';
import { alias, preparePrototype, restoreCapturedSse, restoreResponse } from '../../scripts/maintenance/native-plaintext-collaboration-prototype.ts';

const tool = { type: 'function', name: 'spawn_agent', parameters: { properties: { message: { type: 'string', encrypted: true, description: 'Complete task' } } } };
const namespace = { type: 'namespace', name: 'collaboration', tools: [tool] };
const opaque = { type: 'function_call', namespace: 'collaboration', name: 'spawn_agent', id: 'old', call_id: 'old-call', arguments: '{"message":"opaque"}', encrypted_function_args: ['message'] };

test('prototype adds a separate alias while preserving native schemas, late tools and opaque history', () => {
  const source = { tools: [namespace], input: [opaque, { type: 'additional_tools', tools: [namespace] }] };
  const { request, context } = preparePrototype(source);
  assert.deepEqual(context, { version: 1, alias, tools: ['spawn_agent'] });
  assert.deepEqual(request.tools[0], source.tools[0]);
  assert.deepEqual(request.input[0], opaque);
  assert.deepEqual(request.input[1].tools[0], namespace);
  assert.equal(request.tools[1].name, alias);
  assert.deepEqual(request.tools[1].tools[0].parameters.properties.message, { type: 'string', description: 'Complete task' });
  assert.deepEqual(request.input[1].tools[1], request.tools[1]);
  assert.equal(source.tools.length, 1);
});

test('only complete new alias calls gain the client plaintext marker', () => {
  const fresh = { ...opaque, id: 'new', call_id: 'new-call', namespace: alias, arguments: '{"message":"完整任务\\nalpha beta"}', encrypted_function_args: undefined };
  const response = { output: [opaque, fresh], usage: { output_tokens: 12 }, status: 'completed' };
  const restored = restoreResponse(response, { version: 1, alias, tools: ['spawn_agent'] });
  assert.deepEqual(restored.output[0], opaque);
  assert.equal(restored.output[1].namespace, 'collaboration');
  assert.deepEqual(restored.output[1].encrypted_function_args, []);
  assert.equal(restored.output[1].arguments, fresh.arguments);
  assert.deepEqual(restored.usage, response.usage);
  assert.deepEqual(restoreResponse(response, { version: 1, alias, tools: [] }), response);
  assert.deepEqual(restoreResponse({ ...response, status: 'failed' }, { version: 1, alias, tools: ['spawn_agent'] }), { ...response, status: 'failed' });
});

test('captured SSE restores item identities consistently without touching parameter frames', () => {
  const call = { type: 'function_call', status: 'completed', id: 'new', call_id: 'new-call', namespace: alias, name: 'spawn_agent', arguments: '{"message":"alpha beta"}' };
  const events = [
    { type: 'response.output_item.added', item: { ...call, arguments: '' } },
    { type: 'response.function_call_arguments.delta', item_id: call.id, delta: call.arguments },
    { type: 'response.function_call_arguments.done', item_id: call.id, arguments: call.arguments },
    { type: 'response.output_item.done', item: call },
    { type: 'response.completed', response: { status: 'completed', output: [call, opaque], usage: { output_tokens: 12 } } },
  ];
  const frames = events.map(event => `event: ${event.type}\r\ndata: ${JSON.stringify(event)}\r\n\r\n`);
  const restored = restoreCapturedSse(frames.join(''), { version: 1, alias, tools: ['spawn_agent'] });
  assert.ok(restored.includes(frames[1]!));
  assert.ok(restored.includes(frames[2]!));
  const decoded = restored.trim().split('\r\n\r\n').map(frame => JSON.parse(frame.split('\r\ndata: ')[1]!));
  for (const item of [decoded[0].item, decoded[3].item, decoded[4].response.output[0]]) {
    assert.equal(item.namespace, 'collaboration');
    assert.equal(item.id, call.id);
    assert.equal(item.call_id, call.call_id);
    assert.deepEqual(item.encrypted_function_args, []);
  }
  assert.deepEqual(decoded[4].response.output[1], opaque);
  assert.deepEqual(decoded[4].response.usage, { output_tokens: 12 });
  const emptyTerminalEvents = events.map(event => event.type === 'response.completed'
    ? { ...event, response: { ...event.response, output: [] } }
    : event);
  const emptyCapture = emptyTerminalEvents.map(event => `data: ${JSON.stringify(event)}\n\n`).join('');
  const restoredEmpty = restoreCapturedSse(emptyCapture, { version: 1, alias, tools: ['spawn_agent'] });
  const emptyDecoded = restoredEmpty.trim().split('\n\n').map(frame => JSON.parse(frame.slice(6)));
  assert.equal(emptyDecoded[0].item.namespace, 'collaboration');
  assert.equal(emptyDecoded[3].item.namespace, 'collaboration');
  assert.deepEqual(emptyDecoded[3].item.encrypted_function_args, []);
  assert.deepEqual(emptyDecoded[4].response.output, []);
  assert.equal(emptyDecoded[3].item.arguments, call.arguments);
  assert.deepEqual(emptyDecoded[4].response.usage, { output_tokens: 12 });
  const incompleteDone = emptyTerminalEvents.map(event => event.type === 'response.output_item.done'
    ? { ...event, item: { ...event.item, status: 'incomplete' } }
    : event).map(event => `data: ${JSON.stringify(event)}\n\n`).join('');
  assert.equal(restoreCapturedSse(incompleteDone, { version: 1, alias, tools: ['spawn_agent'] }), incompleteDone);
  assert.equal(restoreCapturedSse(frames.slice(0, 4).join(''), { version: 1, alias, tools: ['spawn_agent'] }), frames.slice(0, 4).join(''));
  const encode = (value: unknown[]) => value.map(event => `data: ${JSON.stringify(event)}\n\n`).join('');
  const invalidCaptures = [
    [...events, { type: 'response.failed', response: { status: 'failed' } }],
    [events[0], events[1], events[4], events[2], events[3]],
    events.map(event => event.type === 'response.completed' ? { ...event, response: { ...event.response, status: 'failed' } } : event),
    [
      { ...events[0], item: { ...call, arguments: 'different' } },
      ...events.slice(2),
    ],
    [
      ...events.slice(0, 4),
      { type: 'response.output_item.added', item: { ...call, id: 'orphan', call_id: 'orphan-call' } },
      events[4],
    ],
    events.map(event => event.type === 'response.completed' ? { ...event, response: { ...event.response, output: [call, { ...opaque, call_id: call.call_id }] } } : event),
  ];
  for (const invalidEvents of invalidCaptures) {
    const invalid = encode(invalidEvents);
    assert.equal(restoreCapturedSse(invalid, { version: 1, alias, tools: ['spawn_agent'] }), invalid);
  }
  const badEvents = structuredClone(events);
  badEvents[1]!.delta = 'different';
  const inconsistent = badEvents.map(event => `event: ${event.type}\r\ndata: ${JSON.stringify(event)}\r\n\r\n`).join('');
  assert.notEqual(inconsistent, frames.join(''));
  assert.equal(restoreCapturedSse(inconsistent, { version: 1, alias, tools: ['spawn_agent'] }), inconsistent);
});

test('a capture with conflicting completed arguments is preserved rather than relabeled plaintext', () => {
  const call = { type: 'function_call', id: 'new', call_id: 'new-call', namespace: alias, name: 'spawn_agent', arguments: '{"message":"task"}' };
  const source = [
    { type: 'response.output_item.done', item: { ...call, arguments: '{"message":"different"}' } },
    { type: 'response.completed', response: { status: 'completed', output: [call] } },
  ].map(event => `data: ${JSON.stringify(event)}\n\n`).join('');
  assert.equal(restoreCapturedSse(source, { version: 1, alias, tools: ['spawn_agent'] }), source);
  assert.deepEqual(restoreResponse({ output: [{ ...call, encrypted_function_args: ['message'] }] }, { version: 1, alias, tools: ['spawn_agent'] }), { output: [{ ...call, encrypted_function_args: ['message'] }] });
});
