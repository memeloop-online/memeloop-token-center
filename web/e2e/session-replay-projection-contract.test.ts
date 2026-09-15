import assert from 'node:assert/strict';
import test from 'node:test';
import {
  projectSessionReplay,
  SESSION_REPLAY_MAX_ITEMS,
} from '../src/sessionReplayProjection.js';
import type { RequestDetail } from '../src/types.js';

function detail(requestId: string, createdAt: number, requestBody: unknown, responseBody: unknown, overrides: Partial<RequestDetail> = {}): RequestDetail {
  return {
    request_id: requestId,
    created_at: createdAt,
    protocol: 'openai',
    model: 'fixture',
    status_code: 200,
    duration_ms: 1,
    input_tokens: 1,
    output_tokens: 1,
    cost: '0',
    error_code: null,
    archive_complete: true,
    request_body: requestBody,
    response_body: responseBody,
    session_context: { session_id: 'session-a', association: 'confirmed', session_name: null, task_kind: null, agent_id: null, semantics_source: 'declared' },
    ...overrides,
  };
}

test('projects Responses text and pairs a cross-request function result only by call_id', () => {
  const first = detail('r1', 1,
    { input: [{ type: 'message', role: 'user', content: [{ type: 'input_text', text: 'find rain' }] }] },
    { output: [
      { type: 'message', role: 'assistant', content: [{ type: 'output_text', text: 'I will check.' }] },
      { type: 'function_call', call_id: 'call-weather', name: 'weather', arguments: '{"city":"Oslo"}' },
    ] },
  );
  const second = detail('r2', 2,
    { input: [{ type: 'function_call_output', call_id: 'call-weather', output: '{"rain":true}' }] },
    { output: [{ type: 'message', role: 'assistant', content: [{ type: 'output_text', text: 'Bring an umbrella.' }] }] },
  );

  const replay = projectSessionReplay('session-a', [second, first]);
  assert.deepEqual(replay.items.filter((item) => item.kind === 'message').map((item) => [item.role, item.text]), [
    ['user', 'find rain'], ['assistant', 'I will check.'], ['assistant', 'Bring an umbrella.'],
  ]);
  const call = replay.items.find((item) => item.kind === 'tool_call');
  const result = replay.items.find((item) => item.kind === 'tool_result');
  assert.deepEqual(call && { callId: call.callId, name: call.name, arguments: call.arguments, pairedResultRequestId: call.pairedResultRequestId, pairing: call.pairing }, {
    callId: 'call-weather', name: 'weather', arguments: '{"city":"Oslo"}', pairedResultRequestId: 'r2', pairing: 'paired',
  });
  assert.deepEqual(result && { callId: result.callId, output: result.output, pairedCallRequestId: result.pairedCallRequestId, pairing: result.pairing }, {
    callId: 'call-weather', output: '{"rain":true}', pairedCallRequestId: 'r1', pairing: 'paired',
  });
});

test('projects Chat Completions messages and does not pair calls by tool name or position', () => {
  const first = detail('chat-1', 1,
    { messages: [{ role: 'user', content: 'translate this' }] },
    { choices: [{ message: { role: 'assistant', content: null, tool_calls: [{ id: 'chat-call', type: 'function', function: { name: 'translate', arguments: '{"text":"hi"}' } }] } }] },
  );
  const second = detail('chat-2', 2,
    { messages: [{ role: 'tool', tool_call_id: 'different-call', name: 'translate', content: 'bonjour' }] },
    { choices: [{ message: { role: 'assistant', content: 'The translation is bonjour.' } }] },
  );

  const replay = projectSessionReplay('session-a', [first, second]);
  const call = replay.items.find((item) => item.kind === 'tool_call');
  const result = replay.items.find((item) => item.kind === 'tool_result');
  assert.equal(call?.pairing, 'unpaired');
  assert.equal(result?.pairing, 'unpaired');
  assert.equal(call?.pairedResultRequestId, null);
  assert.equal(result?.pairedCallRequestId, null);
  assert.deepEqual(replay.items.filter((item) => item.kind === 'message').map((item) => [item.role, item.text, item.unknown]), [
    ['user', 'translate this', null],
    ['assistant', null, 'missing_text'],
    ['assistant', 'The translation is bonjour.', null],
  ]);
});

test('reports unavailable, redacted, and out-of-session data as unknown rather than inventing it', () => {
  const unavailable = detail('missing', 1, null, null, { archive_complete: false });
  const redacted = detail('redacted', 2, { redacted: true }, { output: [{ type: 'message', role: 'assistant', content: { redacted: true } }] });
  const otherSession = detail('other', 3, { input: 'must not be projected' }, { output: [] }, {
    session_context: { session_id: 'session-b', association: 'confirmed', session_name: null, task_kind: null, agent_id: null, semantics_source: 'declared' },
  });

  const replay = projectSessionReplay('session-a', [unavailable, redacted, otherSession]);
  assert.deepEqual(replay.items.filter((item) => item.kind === 'unknown').map((item) => [item.requestId, item.body, item.reason]), [
    ['missing', 'request', 'archive_unavailable'], ['missing', 'response', 'archive_unavailable'],
    ['redacted', 'request', 'redacted'], ['other', 'request', 'outside_session'], ['other', 'response', 'outside_session'],
  ]);
  const redactedMessage = replay.items.find((item) => item.kind === 'message' && item.requestId === 'redacted');
  assert.ok(redactedMessage?.kind === 'message');
  assert.deepEqual({ text: redactedMessage.text, unknown: redactedMessage.unknown }, { text: null, unknown: 'redacted' });
});

test('pages projection items without discarding long retained text or later items', () => {
  const long = detail('long', 1,
    { messages: Array.from({ length: SESSION_REPLAY_MAX_ITEMS + 5 }, (_, index) => ({ role: 'user', content: index === 0 ? 'x'.repeat(20_000) : `message-${index}` })) },
    { choices: [] },
  );
  const replay = projectSessionReplay('session-a', [long]);
  const first = replay.items.find((item) => item.kind === 'message');
  assert.equal(first?.text?.length, 20_000);
  assert.equal(first?.truncated, false);
  assert.equal(replay.truncated, false);
  assert.ok(replay.items.length <= SESSION_REPLAY_MAX_ITEMS);
  assert.equal(replay.nextItemOffset, SESSION_REPLAY_MAX_ITEMS);
  const next = projectSessionReplay('session-a', [long], replay.nextItemOffset!);
  assert.equal(next.items.length, 5);
  assert.equal(next.nextItemOffset, null);
  assert.equal(next.totalItems, SESSION_REPLAY_MAX_ITEMS + 5);
});

test('projects archived Codex custom tools and agent messages without opaque-item floods', () => {
  const archive = detail('codex', 1, { input: [
    { type: 'additional_tools', role: 'developer', tools: [] },
    { type: 'message', role: 'developer', content: 'instructions' },
    ...Array.from({ length: 250 }, () => ({ type: 'reasoning', encrypted_content: 'opaque' })),
    { type: 'agent_message', author: 'worker', content: [{ type: 'input_text', text: 'Found the cause' }] },
    { type: 'custom_tool_call', call_id: 'patch-1', name: 'apply_patch', input: 'patch text' },
    { type: 'custom_tool_call_output', call_id: 'patch-1', output: 'applied' },
  ] }, { output: [{ type: 'message', role: 'assistant', content: 'Done' }] });
  const replay = projectSessionReplay('session-a', [archive]);
  assert.equal(replay.items.filter(item => item.kind === 'unknown').length, 1);
  assert.ok(replay.items.some(item => item.kind === 'message' && item.role === 'agent' && item.text === 'Found the cause'));
  assert.ok(replay.items.some(item => item.kind === 'tool_call' && item.arguments === 'patch text' && item.pairing === 'paired'));
  assert.ok(replay.items.some(item => item.kind === 'tool_result' && item.output === 'applied'));
  assert.ok(replay.items.some(item => item.kind === 'message' && item.role === 'assistant' && item.text === 'Done'));
  assert.equal(replay.truncated, false);
});

test('removes only carried history prefixes and preserves a repeated new user turn', () => {
  const user = { type: 'message', role: 'user', content: 'Again' };
  const assistant = { type: 'message', role: 'assistant', content: 'Done' };
  const first = detail('history-1', 1, { input: [user] }, { output: [assistant] });
  const second = detail('history-2', 2, { input: [user, assistant, user] }, { output: [] });
  const replay = projectSessionReplay('session-a', [first, second]);
  assert.deepEqual(replay.items.filter(item => item.kind === 'message').map(item => [item.role, item.text]), [
    ['user', 'Again'], ['assistant', 'Done'], ['user', 'Again'],
  ]);
});

test('reads retained Responses SSE terminal output and ordered completed tool items', () => {
  const stream = [
    'event: response.output_item.done\r\ndata: {"type":"response.output_item.done","output_index":1,"item":{"type":"custom_tool_call","call_id":"c","name":"patch","input":"actual patch"}}',
    'data: {"type":"response.output_item.done","output_index":0,"item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Working"}]}}',
    'data: {"type":"response.completed","response":{"output":[]}}',
    'data: [DONE]',
  ].join('\r\n\r\n');
  const replay = projectSessionReplay('session-a', [detail('sse', 1, { input: 'Task' }, stream)]);
  assert.deepEqual(replay.items.map(item => item.kind), ['message', 'message', 'tool_call']);
  assert.ok(replay.items.some(item => item.kind === 'message' && item.role === 'assistant' && item.text === 'Working'));
  assert.ok(replay.items.some(item => item.kind === 'tool_call' && item.arguments === 'actual patch'));
  const partial = projectSessionReplay('session-a', [detail('partial', 1, { input: 'Task' }, stream.split('data: {"type":"response.completed"')[0])]);
  assert.ok(partial.items.some(item => item.kind === 'unknown' && item.body === 'response' && item.reason === 'archive_unavailable'));
});
