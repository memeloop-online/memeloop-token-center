import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { AppServer, bridge, delegateTool } from '../../scripts/maintenance/plaintext-host-bridge.ts';

test('isolated dynamic-tool delegation forwards exact plaintext into a distinct Kimi thread', async () => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'mtc-host-bridge-'));
  const directory = path.join(root, 'workspace'); await fs.mkdir(directory);
  const calls: any[] = [];
  let closed = false, starts = 0;
  const fake = {
    notification: (_message: any) => {}, disconnected: (_error: Error) => {},
    request: async (_message: any): Promise<any> => {}, send: (_message: unknown) => {},
    close: async () => { closed = true; },
    call: async (method: string, params: any) => {
      calls.push({ method, params });
      if (method === 'thread/start') return { thread: { id: `thread-${++starts}` } };
      if (method === 'turn/start') {
        setImmediate(async () => {
          if (params.threadId === 'thread-1') {
            const response = await fake.request({ method: 'item/tool/call', params: { threadId: 'thread-1', turnId: 'turn-parent', callId: 'delegate-call', tool: delegateTool, arguments: { task: 'Review exact task αβ' } } });
            assert.equal(response.success, true);
          }
          fake.notification({ method: 'item/completed', params: { threadId: params.threadId, item: { type: 'agentMessage', id: 'message', text: 'Actual review result' } } });
          fake.notification({ method: 'turn/completed', params: { threadId: params.threadId, turn: { id: 'turn', status: 'completed', items: [{ type: 'reasoning', text: 'not public' }] } } });
        });
      }
      return {};
    },
  };
  try {
    const result = await bridge({ binary: 'unused', baseUrl: 'https://example.invalid/v1', apiKey: 'private-key', parentModel: 'parent', kimiModel: 'kimi', directory, receipt: path.join(root, 'receipt.json'), task: 'Useful task' }, () => fake as unknown as AppServer);
    assert.equal(result.completed, true); assert(closed);
    const threads = calls.filter(c => c.method === 'thread/start');
    assert.equal(threads[0].params.dynamicTools[0].name, delegateTool);
    assert.equal(threads[1].params.model, 'kimi');
    assert.equal(threads[1].params.dynamicTools, undefined);
    const turns = calls.filter(c => c.method === 'turn/start');
    assert.deepEqual(turns[1].params.input, [{ type: 'text', text: 'Review exact task αβ' }]);
    assert.equal(result.delegations[0].callId, 'delegate-call');
    const receipt = await fs.readFile(path.join(root, 'receipt.json'), 'utf8');
    assert(!receipt.includes('private-key')); assert(!receipt.includes('not public'));
  } finally { await fs.rm(root, { recursive: true, force: true }); }
});
