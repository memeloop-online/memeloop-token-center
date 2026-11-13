#!/usr/bin/env node
import { createInterface } from 'node:readline';

// Static, network-free app-server peer for the actual --serve CLI lifecycle.
let threads = 0, turns = 0, request = 100;
const parentTurns = new Map<number, { threadId: string; id: string }>();
const send = (message: unknown) => process.stdout.write(JSON.stringify(message) + '\n');
const finish = (threadId: string, id: string, status = 'completed') => {
  send({ method: 'item/completed', params: { threadId, turnId: id, item: { id: `message-${id}`, type: 'agentMessage', text: 'Visible result' } } });
  send({ method: 'turn/completed', params: { threadId, turn: { id, status } } });
};
const lines = createInterface({ input: process.stdin });
lines.on('line', line => {
  const message = JSON.parse(line);
  if (message.method === 'initialize') send({ id: message.id, result: {} });
  if (message.method === 'thread/start') send({ id: message.id, result: { thread: { id: `thread-${++threads}` } } });
  if (message.method === 'turn/start') {
    const { threadId, input } = message.params;
    const id = `turn-${++turns}`;
    send({ id: message.id, result: { turn: { id } } });
    if (threadId === 'thread-1') {
      const call = ++request;
      parentTurns.set(call, { threadId, id });
      send({ id: call, method: 'item/tool/call', params: { threadId, turnId: id, callId: `call-${id}`, tool: 'delegate_plaintext_kimi', arguments: { task: input[0].text.split('\n\n').slice(1).join('\n\n') } } });
      if (process.env.MTC_FAKE_PARENT_TERMINAL) finish(threadId, id, process.env.MTC_FAKE_PARENT_TERMINAL);
    } else {
      // Force the parent terminal notification to race a still-running child.
      setTimeout(() => finish(threadId, id), 20);
    }
  }
  if (message.result && parentTurns.has(message.id)) {
    const parent = parentTurns.get(message.id)!;
    parentTurns.delete(message.id);
    if (!process.env.MTC_FAKE_PARENT_TERMINAL) finish(parent.threadId, parent.id);
  }
});
