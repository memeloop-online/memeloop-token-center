import assert from 'node:assert/strict';
import test from 'node:test';
import { readFile } from 'node:fs/promises';
import {
  appendContextChunk, contextMessageText, createSessionContextLoader, prependContextPage,
  validateContextChunk, validateContextPage, type SessionContextPage,
} from '../src/sessionContextReader.js';

const node = (depth: number) => depth.toString(16).padStart(64, '0');
function page(first: number, last: number): SessionContextPage {
  return {
    contract_version: 'session_context_v1', representation: 'semantic_context',
    session_id: 's', request_id: 'r', next_cursor: first > 1 ? `before-${first}` : null,
    items: Array.from({ length: last - first + 1 }, (_, index) => ({
      node_id: node(first + index), depth: first + index, role: 'user', kind: 'message',
      phase: null, text: 'intentional repeat', text_complete: true, source_chars: 50,
      content_cursor: `content-${first + index}`,
    })),
  };
}

test('context paging preserves >1024 entries and intentional repeated text without implicit reads', () => {
  let current = validateContextPage(page(1025, 1056), 's', 'r');
  for (let end = 1024; end > 0; end -= 32) {
    current = prependContextPage(current, validateContextPage(page(end - 31, end), 's', 'r'));
  }
  assert.equal(current.items.length, 1056);
  assert.equal(current.next_cursor, null);
  assert.equal(current.items[0].text, current.items.at(-1)?.text);
  assert.notEqual(current.items[0].node_id, current.items.at(-1)?.node_id);
  assert.throws(() => prependContextPage(current, page(1, 32)));
});

test('wire and merge guards reject mismatched sessions, requests, gaps and repeated positions', () => {
  assert.throws(() => validateContextPage(page(1, 32), 'other', 'r'));
  assert.throws(() => validateContextPage(page(1, 32), 's', 'other'));
  assert.throws(() => validateContextPage(page(1, 33), 's', 'r'));
  const bad = page(1, 3);
  bad.items[2].depth = 4;
  assert.throws(() => validateContextPage(bad, 's', 'r'));
  assert.throws(() => prependContextPage(page(33, 64), page(1, 31)));
  assert.throws(() => prependContextPage(page(33, 64), { ...page(1, 32), request_id: 'reset' }));
});

test('content chunks validate scalar offsets and cannot mix nodes or versions', () => {
  const first = validateContextChunk({
    contract_version: 'session_context_content_v1', representation: 'semantic_context',
    node_id: node(1), offset_chars: 0, total_chars: 4, content: '中😀', next_cursor: 'next',
  });
  const state = appendContextChunk(undefined, node(1), first);
  assert.equal(state.loadedChars, 2);
  const last = { ...first, offset_chars: 2, content: '文🎉', next_cursor: null };
  const complete = appendContextChunk(state, node(1), validateContextChunk(last));
  assert.equal(complete.chunks.join(''), '中😀文🎉');
  assert.equal(complete.loadedChars, 4);
  assert.throws(() => appendContextChunk(state, node(2), last));
  assert.throws(() => appendContextChunk(state, node(1), { ...last, total_chars: 5 }));
  assert.throws(() => validateContextChunk({ ...first, content: '' }));
  assert.throws(() => validateContextChunk({ ...last, offset_chars: 3 }));
});

test('mixed user content never promotes embedded tool results or media to visible user text', () => {
  assert.equal(contextMessageText({ content: [
    { type: 'text', text: 'Actual user message' },
    { type: 'tool_result', content: 'Very large output' },
    { type: 'image', source: { data: 'base64' } },
  ] }), 'Actual user message');
});

test('constructing and paging a loader does not fetch content or follow continuation automatically', async () => {
  const original = globalThis.fetch;
  const requests: string[] = [];
  globalThis.fetch = async (input) => {
    requests.push(String(input));
    return new Response(JSON.stringify(page(33, 64)), { status: 200, headers: { 'Content-Type': 'application/json' } });
  };
  try {
    const loader = createSessionContextLoader('s', 'test', (request, content) => `/scope/${request}/${content ? 'content' : 'page'}?tenant=one`);
    assert.equal(requests.length, 0);
    await loader.page('r', undefined, false, new AbortController().signal);
    assert.equal(requests.length, 1);
    assert.match(requests[0], /tenant=one&details=false/);
    assert.doesNotMatch(requests[0], /content|cursor/);
  } finally { globalThis.fetch = original; }
});

test('content disclosure mounts its reader only on expansion and offers bounded continuation', async () => {
  const source = await readFile(new URL('../src/SessionContextContent.tsx', import.meta.url), 'utf8');
  assert.match(source, /open && <SessionContextContent/);
  assert.match(source, /<Disclosure title=\{label\} open=\{open\} onOpenChange=\{setOpen\}/);
  assert.doesNotMatch(source, /<details|<summary/);
  assert.match(source, /loader\.content\(requestId, requested, controller\.signal\)/);
  assert.match(source, /setRequested\(state\.nextCursor!\)/);
  assert.match(source, /controller\.abort\(\)/);
  assert.doesNotMatch(source.split('export function SessionUserText')[0], /while\s*\(/);
  assert.doesNotMatch(source, /dangerouslySetInnerHTML|\.innerHTML/);
});

test('a content continuation cannot change from text-only to raw tool-capable JSON', () => {
  const first = validateContextChunk({
    contract_version: 'session_context_content_v1', representation: 'message_text',
    node_id: node(1), offset_chars: 0, total_chars: 4, content: '中😀', next_cursor: 'next',
  });
  const state = appendContextChunk(undefined, node(1), first);
  assert.throws(() => appendContextChunk(state, node(1), {
    ...first, representation: 'semantic_context', offset_chars: 2, content: '文🎉', next_cursor: null,
  }));
});

test('response status polling is scoped and skips context pages and archive content', async () => {
  const original = globalThis.fetch;
  const requests: string[] = [];
  globalThis.fetch = async (input) => {
    requests.push(String(input));
    return new Response(JSON.stringify({ ...page(1, 1), items: [], response: {
      state: 'ready', text: 'Final', node_id: node(9), text_cursor: 'text', total_chars: 5, error_code: null,
    } }));
  };
  try {
    const loader = createSessionContextLoader('s', 'test', () => '/context?key_id=owned');
    const response = await loader.response!('r', new AbortController().signal);
    assert.equal(response.text, 'Final');
    assert.equal(requests.length, 1);
    assert.match(requests[0], /key_id=owned&response_only=true/);
    const malformed = { ...page(1, 1), response: { ...response, total_chars: 1 } };
    assert.throws(() => validateContextPage(malformed, 's', 'r'));
  } finally { globalThis.fetch = original; }
});

test('context pages, response polling and content share one four-read concurrency bound', async () => {
  const original = globalThis.fetch;
  const releases: Array<() => void> = [];
  let active = 0;
  let maximum = 0;
  globalThis.fetch = async (input) => {
    active += 1;
    maximum = Math.max(maximum, active);
    await new Promise<void>((resolve) => releases.push(resolve));
    active -= 1;
    const value = String(input).includes('/content')
      ? { contract_version: 'session_context_content_v1', representation: 'message_text',
        node_id: node(1), offset_chars: 0, total_chars: 1, content: '文', next_cursor: null }
      : { ...page(1, 1), response: { state: 'pending', text: null, node_id: null,
        text_cursor: null, total_chars: 0, error_code: null } };
    return new Response(JSON.stringify(value));
  };
  try {
    const loader = createSessionContextLoader('s', 'test', (_request, content) => content ? '/content' : '/context');
    const signal = new AbortController().signal;
    const reads = [
      loader.page('r', undefined, false, signal),
      loader.response!('r', signal),
      loader.content('r', 'cursor', signal),
      loader.page('r', undefined, false, signal),
      loader.response!('r', signal),
      loader.content('r', 'cursor', signal),
    ];
    // Yield through fetch/API promises, without a wall-clock sleep.
    await new Promise<void>((resolve) => setImmediate(resolve));
    assert.equal(releases.length, 4);
    while (releases.length) {
      releases.shift()!();
      await new Promise<void>((resolve) => setImmediate(resolve));
    }
    await Promise.all(reads);
    assert.equal(maximum, 4);
  } finally {
    for (const release of releases) release();
    globalThis.fetch = original;
  }
});
