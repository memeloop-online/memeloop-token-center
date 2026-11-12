import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import fs from 'node:fs/promises';
import { constants } from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

export const delegateTool = 'delegate_plaintext_kimi';
export const delegateSpec = { type: 'function', name: delegateTool, description: 'Delegate a complete readable task to a separate Kimi thread and return its real result.', inputSchema: { type: 'object', properties: { task: { type: 'string' } }, required: ['task'], additionalProperties: false } };

/** Own stdio app-server only; never connects to or replaces an existing desktop session. */
export class AppServer {
  process;
  nextId = 0;
  pending = new Map<number, { resolve: (value: any) => void; reject: (error: Error) => void }>();
  notification: (message: any) => void = () => {};
  disconnected: (error: Error) => void = () => {};
  request: (message: any) => Promise<any> = async () => { throw new Error('Unsupported server request'); };
  constructor(binary: string, args: string[], env: NodeJS.ProcessEnv) {
    this.process = spawn(binary, [...args, 'app-server'], { env, stdio: ['pipe', 'pipe', 'pipe'] });
    this.process.stderr.resume(); // Never print provider credentials or unfiltered server diagnostics.
    const lines = createInterface({ input: this.process.stdout });
    lines.on('line', line => {
      try {
        const message = JSON.parse(line);
        if (message.method && message.id !== undefined) {
          void this.request(message).then(result => this.send({ id: message.id, result }), () => this.send({ id: message.id, error: { code: -32603, message: 'Bridge request failed; inspect private receipt.' } }));
        } else if (message.method) this.notification(message);
        else {
          const pending = this.pending.get(message.id);
          this.pending.delete(message.id);
          if (message.error) pending?.reject(new Error(JSON.stringify(message.error)));
          else pending?.resolve(message.result);
        }
      } catch { this.fail(new Error('Invalid app-server JSON')); }
    });
    this.process.once('error', () => this.fail(new Error('App-server could not start')));
    this.process.once('exit', () => this.fail(new Error('App-server exited')));
  }
  fail(error: Error) { for (const pending of this.pending.values()) pending.reject(error); this.pending.clear(); this.disconnected(error); }
  send(message: unknown) { this.process.stdin.write(JSON.stringify(message) + '\n'); }
  call(method: string, params: unknown): Promise<any> {
    const id = ++this.nextId;
    return new Promise((resolve, reject) => { this.pending.set(id, { resolve, reject }); this.send({ id, method, params }); });
  }
  async close() {
    if (!this.process.pid || this.process.exitCode !== null || this.process.signalCode !== null) return;
    const exited = new Promise(resolve => this.process.once('exit', resolve));
    this.process.stdin.end();
    this.process.kill('SIGTERM');
    await exited;
  }
}

type Options = { binary: string; baseUrl: string; apiKey: string; parentModel: string; kimiModel: string; directory: string; receipt: string; task: string };

export async function bridge(options: Options, connect = (binary: string, args: string[], env: NodeJS.ProcessEnv) => new AppServer(binary, args, env)) {
  const cwd = await fs.realpath(options.directory);
  const receipt = path.resolve(options.receipt);
  const receiptParent = await fs.realpath(path.dirname(receipt));
  if (receiptParent === cwd || receiptParent.startsWith(cwd + path.sep)) throw new Error('Keep receipt outside the isolated workspace.');
  const record: any = { task: options.task, cwd, mode: 'isolated-app-server-dynamic-tool', native_collaboration_modified: false, threads: [], delegations: [], completed: false };
  const save = async () => {
    const file = await fs.open(receipt, constants.O_WRONLY | constants.O_CREAT | constants.O_TRUNC | constants.O_NOFOLLOW, 0o600);
    try { await file.chmod(0o600); await file.writeFile(JSON.stringify(record, null, 2).replaceAll(options.apiKey, '[redacted]') + '\n'); }
    finally { await file.close(); }
  };
  await save();
  const provider = 'mtc_plaintext_bridge';
  const args = ['-c', `model_provider=${JSON.stringify(provider)}`, '-c', `model_providers.${provider}.name="MTC plaintext bridge"`, '-c', `model_providers.${provider}.base_url=${JSON.stringify(options.baseUrl)}`, '-c', `model_providers.${provider}.wire_api="responses"`, '-c', `model_providers.${provider}.env_key="MTC_BRIDGE_API_KEY"`];
  const server = connect(options.binary, args, { ...process.env, MTC_BRIDGE_API_KEY: options.apiKey });
  const turns = new Map<string, { resolve: (turn: any) => void; reject: (error: Error) => void; items: any[] }>();
  server.disconnected = error => { for (const turn of turns.values()) turn.reject(error); turns.clear(); };
  server.notification = message => {
    const params = message.params;
    const pending = turns.get(params?.threadId);
    if (!pending) return;
    if (message.method === 'item/completed' && params.item?.type === 'agentMessage') pending.items.push({ id: params.item.id, text: params.item.text });
    if (message.method === 'turn/completed') {
      turns.delete(params.threadId);
      pending.resolve({ id: params.turn.id, status: params.turn.status, error: params.turn.error, visibleMessages: pending.items });
    }
  };
  const run = async (model: string, task: string, dynamicTools?: unknown[], onStarted?: (threadId: string) => void) => {
    const started = await server.call('thread/start', { cwd, model, modelProvider: provider, approvalPolicy: 'never', sandbox: 'read-only', ephemeral: true, ...(dynamicTools ? { dynamicTools } : {}) });
    const threadId = started.thread.id;
    onStarted?.(threadId);
    record.threads.push({ threadId, model, task }); await save();
    const completed = new Promise<any>((resolve, reject) => turns.set(threadId, { resolve, reject, items: [] }));
    // If turn/start fails while the subprocess also disconnects, consume both
    // outcomes without leaving a detached rejected completion promise.
    void completed.catch(() => {});
    await server.call('turn/start', { threadId, input: [{ type: 'text', text: task }] });
    const result = await completed;
    record.threads.find((thread: any) => thread.threadId === threadId).result = result;
    await save();
    return { threadId, result };
  };
  server.request = async message => {
    const p = message.params;
    if (message.method !== 'item/tool/call' || p.tool !== delegateTool || p.namespace != null || typeof p.arguments?.task !== 'string') throw new Error('Unsupported dynamic tool request');
    const parent = record.threads[0];
    if (!parent || p.threadId !== parent.threadId) throw new Error('Delegation must originate from this bridge parent');
    const delegation: any = { parentThreadId: p.threadId, turnId: p.turnId, callId: p.callId, task: p.arguments.task };
    record.delegations.push(delegation); await save();
    try {
      const child = await run(options.kimiModel, p.arguments.task, undefined, id => { delegation.childThreadId = id; });
      delegation.result = child.result; delegation.status = child.result.status; await save();
      return { success: child.result.status === 'completed', contentItems: [{ type: 'inputText', text: JSON.stringify(child).replaceAll(options.apiKey, '[redacted]') }] };
    } catch (error) {
      delegation.status = 'failed'; delegation.error = error instanceof Error ? error.message : 'child_failed'; await save();
      return { success: false, contentItems: [{ type: 'inputText', text: JSON.stringify(delegation).replaceAll(options.apiKey, '[redacted]') }] };
    }
  };
  try {
    await server.call('initialize', { clientInfo: { name: 'mtc-isolated-plaintext-bridge', version: '1' }, capabilities: { experimentalApi: true } });
    server.send({ method: 'initialized', params: {} });
    const parent = await run(options.parentModel, `Use ${delegateTool} to ask Kimi to perform the following complete task, then report its actual result. Do not use collaboration spawn_agent or opaque handoff.\n\n${options.task}`, [delegateSpec]);
    record.completed = parent.result.status === 'completed' && record.delegations.length > 0 && record.delegations.every((d: any) => d.result?.status === 'completed');
    await save(); return JSON.parse(JSON.stringify(record).replaceAll(options.apiKey, '[redacted]'));
  } catch (error) {
    record.error = error instanceof Error ? error.message : 'bridge_failed'; await save(); return JSON.parse(JSON.stringify(record).replaceAll(options.apiKey, '[redacted]'));
  } finally { await server.close(); }
}

async function main() {
  const { MTC_BASE_URL: baseUrl, MTC_API_KEY: apiKey, MTC_TASK_DIRECTORY: directory, MTC_TASK_ARCHIVE: receipt } = process.env;
  if (!baseUrl || !apiKey || !directory || !receipt) throw new Error('Set explicit gateway, downstream key, isolated directory and private receipt.');
  let task = ''; for await (const chunk of process.stdin) task += chunk;
  const result = await bridge({ baseUrl, apiKey, directory, receipt, task, binary: process.env.MTC_CODEX_BINARY ?? 'codex', parentModel: process.env.MTC_PARENT_MODEL ?? 'gpt-6.1-sol', kimiModel: process.env.MTC_TASK_MODEL ?? 'k3.1-pro' });
  console.log(JSON.stringify(result)); if (!result.completed) process.exitCode = 1;
}
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) main().catch(() => { console.error('Isolated bridge did not complete.'); process.exitCode = 1; });
