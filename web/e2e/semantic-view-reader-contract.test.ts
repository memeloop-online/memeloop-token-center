import assert from 'node:assert/strict';
import test from 'node:test';
import { exportSemanticContext, type SemanticViewLoader, type SemanticViewPage } from '../src/semanticViewReader.js';
import { semanticMessageId } from '../src/semanticConversationIdentity.js';

const instance = { instance_id: 'occurrence-a', request_id: 'request-a', branch_id: 'branch-a', ordinal: 1, atom_hash: 'same-atom', node_hash: 'node-a', atom_cursor: 'signed-a', origin: 'request', terminal_state: 'complete' };
const page: SemanticViewPage = { status: 'published', manifest_revision: '18446744073709551615',
  coverage: { observed_terminal: true, projection_state: 'complete', gaps: [], omissions: [] }, instances: [instance], next_cursor: null };

test('identical atom content preserves separate occurrence and revision identities', () => {
  assert.notEqual(semanticMessageId('3', instance), semanticMessageId('3', { ...instance, instance_id: 'occurrence-b' }));
  assert.notEqual(semanticMessageId('3', instance), semanticMessageId('4', instance));
});

test('stream export aborts a revision change instead of committing a mixed transcript', async () => {
  const previousWindow = Object.getOwnPropertyDescriptor(globalThis, 'window');
  let writes = 0; let aborted = false; let closed = false;
  Object.defineProperty(globalThis, 'window', { configurable: true, value: {
    showSaveFilePicker: async () => ({ createWritable: async () => ({
      write: async () => { writes++; }, close: async () => { closed = true; }, abort: async () => { aborted = true; },
    }) }),
  } });
  let calls = 0;
  const loader: SemanticViewLoader = {
    page: async () => calls++ === 0 ? { ...page, next_cursor: 'older' } : { ...page, manifest_revision: '18446744073709551614' },
    atom: async () => new Response('{"role":"assistant","parts":[]}'),
  };
  try {
    await assert.rejects(exportSemanticContext(loader, 'request-a', new AbortController().signal), /revision changed/);
    assert.ok(writes > 0); assert.equal(aborted, true); assert.equal(closed, false);
  } finally {
    if (previousWindow) Object.defineProperty(globalThis, 'window', previousWindow);
    else Reflect.deleteProperty(globalThis, 'window');
  }
});

test('pending semantic publication cannot be exported as a complete empty conversation', async () => {
  const previousWindow = Object.getOwnPropertyDescriptor(globalThis, 'window');
  let aborted = false;
  Object.defineProperty(globalThis, 'window', { configurable: true, value: { showSaveFilePicker: async () => ({ createWritable: async () => ({
    write: async () => assert.fail('pending content must not be written'), close: async () => assert.fail('must not commit'), abort: async () => { aborted = true; },
  }) }) } });
  try {
    await assert.rejects(exportSemanticContext({ page: async () => ({ ...page, status: 'pending', manifest_revision: null, instances: [] }), atom: async () => assert.fail('must not read atom') }, 'request-a', new AbortController().signal), /pending/);
    assert.equal(aborted, true);
  } finally {
    if (previousWindow) Object.defineProperty(globalThis, 'window', previousWindow);
    else Reflect.deleteProperty(globalThis, 'window');
  }
});
