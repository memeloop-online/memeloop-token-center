import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { AppServer, bridge, delegateTool, serve } from '../../scripts/maintenance/plaintext-host-bridge.ts';

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
  const peer = fileURLToPath(new URL('./fixtures/plaintext-host-bridge-peer.ts', import.meta.url));
  const server = new AppServer(process.execPath, [peer], process.env);
  try {
    assert.deepEqual(await server.call('initialize', {}), { ok: true });
    const finished = new Promise(resolve => { server.notification = m => resolve(m.params); });
    server.request = async m => ({ echoed: m.params.task });
    server.send({ method: 'ask', params: {} });
    assert.deepEqual(await finished, { echoed: 'plain' });
  } finally { await server.close(); }
});


test('persistent bridge reuses only its parent and journals distinct plaintext tasks once', async () => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'mtc-host-session-'));
  const directory = path.join(root, 'workspace'); await fs.mkdir(directory);
  const receipt = path.join(root, 'session.ndjson');
  const outputs: any[] = [], calls: any[] = [];
  let threads = 0, turns = 0, closed = false;
  const tasks = ['Review Unicode α', 'Review next β'];
  const fake = {
    notification: (_m: any) => {}, disconnected: (_e: Error) => {},
    request: async (_m: any): Promise<any> => {}, send: (_m: unknown) => {},
    close: async () => { closed = true; },
    call: async (method: string, params: any) => {
      calls.push({ method, params });
      if (method === 'thread/start') return { thread: { id: `thread-${++threads}` } };
      if (method === 'turn/start') {
        const id = `turn-${++turns}`;
        const task = tasks[outputs.length];
        setImmediate(async () => {
          if (params.threadId === 'thread-1') {
            const response = await fake.request({ method: 'item/tool/call', params: { threadId: params.threadId, turnId: id, callId: `call-${id}`, tool: delegateTool, arguments: { task } } });
            assert.equal(response.success, true);
          }
          fake.notification({ method: 'item/completed', params: { threadId: params.threadId, turnId: id, item: { type: 'agentMessage', id: `message-${id}`, text: 'Actual visible result' } } });
          fake.notification({ method: 'turn/completed', params: { threadId: params.threadId, turn: { id, status: 'completed' } } });
        });
        return { turn: { id } };
      }
      return {};
    },
  };
  async function* commands() { for (const [i, task] of tasks.entries()) yield JSON.stringify({ id: `task-${i}`, task }); }
  try {
    await serve({ binary: 'unused', baseUrl: 'https://example.invalid/v1', apiKey: 'private-key', parentModel: 'parent', kimiModel: 'kimi', directory, receipt }, commands(), result => outputs.push(result), () => fake as unknown as AppServer);
    assert(closed); assert.equal(outputs.length, 2); assert(outputs.every(result => result.completed));
    const starts = calls.filter(call => call.method === 'thread/start');
    assert.equal(starts.length, 3); assert.equal(starts.filter(call => call.params.dynamicTools).length, 1);
    const childTurns = calls.filter(call => call.method === 'turn/start' && call.params.threadId !== 'thread-1');
    assert.deepEqual(childTurns.map(call => call.params.input[0].text), tasks);
    const text = await fs.readFile(receipt, 'utf8');
    const events = text.trim().split('\n').map(line => JSON.parse(line));
    assert.equal(events.filter(event => event.type === 'task').length, 2);
    assert.equal(events.filter(event => event.type === 'delegation' && event.taskRef).length, 2);
    assert.equal(events.filter(event => event.type === 'call_return').length, 2);
    for (const task of tasks) assert.equal(text.split(task).length - 1, 1);
    assert(!text.includes('private-key'));
    assert.equal(events.at(-1).type, 'session_end');
  } finally { await fs.rm(root, { recursive: true, force: true }); }
});

test('persistent bridge cancellation interrupts its owned turn and does not execute the next task', async () => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'mtc-host-cancel-'));
  const directory = path.join(root, 'workspace'); await fs.mkdir(directory);
  const controller = new AbortController();
  const outputs: any[] = [], calls: any[] = [];
  const fake = {
    notification: (_m: any) => {}, disconnected: (_e: Error) => {}, request: async (_m: any): Promise<any> => {}, send: (_m: unknown) => {},
    close: async () => { fake.disconnected(new Error('Owned app-server closed')); },
    call: async (method: string, params: any) => {
      calls.push({ method, params });
      if (method === 'thread/start') return { thread: { id: 'owned-parent' } };
      if (method === 'turn/start') { setImmediate(() => controller.abort()); return { turn: { id: 'owned-turn' } }; }
      return {};
    },
  };
  async function* commands() { yield JSON.stringify({ id: 'first', task: 'Read only' }); yield JSON.stringify({ id: 'second', task: 'Must not execute' }); }
  try {
    await serve({ binary: 'unused', baseUrl: 'https://example.invalid/v1', apiKey: 'private-key', parentModel: 'parent', kimiModel: 'kimi', directory, receipt: path.join(root, 'session.ndjson') }, commands(), result => outputs.push(result), () => fake as unknown as AppServer, controller.signal);
    assert.equal(outputs.length, 1); assert.equal(outputs[0].status, 'interrupted'); assert.equal(outputs[0].outcome, 'unknown');
    assert.equal(calls.filter(call => call.method === 'turn/start').length, 1);
    assert.deepEqual(calls.find(call => call.method === 'turn/interrupt').params, { threadId: 'owned-parent', turnId: 'owned-turn' });
  } finally { await fs.rm(root, { recursive: true, force: true }); }
});
