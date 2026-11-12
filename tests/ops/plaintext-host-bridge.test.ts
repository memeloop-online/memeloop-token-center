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
    const result = await bridge({ binary: 'unused', baseUrl: 'https://example.invalid/v1', apiKey: 'private-key', parentModel: 'parent', kimiModel: 'kimi', directory, receipt: path.join(root, 'receipt.json'), task: 'Useful task with private-key' }, (_binary, args) => {
      assert(args.includes('model_provider="mtc_plaintext_bridge"'));
      return fake as unknown as AppServer;
    });
    assert.equal(result.completed, true); assert(closed);
    assert(!JSON.stringify(result).includes('private-key'));
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

test('a child turn-start failure retains child identity, call identity and explicit failed result', async () => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'mtc-host-failure-'));
  const directory = path.join(root, 'workspace'); await fs.mkdir(directory);
  let starts = 0;
  const fake = {
    notification: (_m: any) => {}, disconnected: (_e: Error) => {}, request: async (_m: any): Promise<any> => {}, send: (_m: unknown) => {}, close: async () => {},
    call: async (method: string, params: any) => {
      if (method === 'thread/start') return { thread: { id: `thread-${++starts}` } };
      if (method === 'turn/start' && params.threadId === 'thread-2') throw new Error('Child failed private-key');
      if (method === 'turn/start') setImmediate(async () => {
        await fake.request({ method: 'item/tool/call', params: { threadId: 'thread-1', turnId: 'parent-turn', callId: 'call-one', tool: delegateTool, arguments: { task: 'Exact task' } } });
        fake.notification({ method: 'turn/completed', params: { threadId: 'thread-1', turn: { id: 'parent-turn', status: 'completed' } } });
      });
      return {};
    },
  };
  try {
    const result = await bridge({ binary: 'unused', baseUrl: 'https://example.invalid/v1', apiKey: 'private-key', parentModel: 'parent', kimiModel: 'kimi', directory, receipt: path.join(root, 'receipt.json'), task: 'Task' }, () => fake as unknown as AppServer);
    assert.equal(result.completed, false);
    assert.equal(result.delegations[0].childThreadId, 'thread-2');
    assert.equal(result.delegations[0].callId, 'call-one');
    assert.equal(result.delegations[0].status, 'failed');
    assert.match(result.delegations[0].error, /Child failed/);
    assert(!JSON.stringify(result).includes('private-key'));
  } finally { await fs.rm(root, { recursive: true, force: true }); }
});

test('AppServer transports actual subprocess JSON lines and asynchronous server requests', async () => {
  // A real subprocess protocol peer, not the real Codex/model service. Live host
  // acceptance is deliberately a separate operational gate after source review.
  const peer = `const r=require('node:readline').createInterface({input:process.stdin});
    r.on('line',l=>{const m=JSON.parse(l);if(m.method==='initialize')process.stdout.write(JSON.stringify({id:m.id,result:{ok:true}})+'\\n');
    if(m.method==='ask')process.stdout.write(JSON.stringify({id:99,method:'item/tool/call',params:{task:'plain'}})+'\\n');
    if(m.id===99&&m.result)process.stdout.write(JSON.stringify({method:'peer/done',params:m.result})+'\\n');});`;
  const server = new AppServer(process.execPath, ['-e', peer], process.env);
  try {
    assert.deepEqual(await server.call('initialize', {}), { ok: true });
    const finished = new Promise(resolve => { server.notification = m => resolve(m.params); });
    server.request = async m => ({ echoed: m.params.task });
    server.send({ method: 'ask', params: {} });
    assert.deepEqual(await finished, { echoed: 'plain' });
  } finally { await server.close(); }
});
