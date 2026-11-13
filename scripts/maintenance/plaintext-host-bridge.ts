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
  stopped = false;
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
  fail(error: Error) { this.stopped = true; for (const pending of this.pending.values()) pending.reject(error); this.pending.clear(); this.disconnected(error); }
  send(message: unknown) { if (this.stopped) return; this.process.stdin.write(JSON.stringify(message) + '\n'); }
  call(method: string, params: unknown): Promise<any> {
    if (this.stopped) return Promise.reject(new Error('App-server is closed'));
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

type SessionOptions = Omit<Options, 'task'>;
type TurnResult = { id: string; status: string; error?: unknown; visibleMessages: { id: string; text: string }[] };

// One isolated parent, a caller-owned input stream, and an append-only receipt.
// Only the current task/result is held here; previous task bodies are not copied
// into subsequent receipt events or retained as a second conversation history.
export async function serve(
  options: SessionOptions,
  commands: AsyncIterable<string>,
  output: (result: any) => void,
  connect = (binary: string, args: string[], env: NodeJS.ProcessEnv) => new AppServer(binary, args, env),
  signal?: AbortSignal,
) {
  // Subscribe before filesystem setup can yield and a pre-piped stdin reaches EOF.
  const iterator = commands[Symbol.asyncIterator]();
  const cwd = await fs.realpath(options.directory);
  const receipt = path.resolve(options.receipt);
  const parent = await fs.realpath(path.dirname(receipt));
  if (parent === cwd || parent.startsWith(cwd + path.sep)) throw new Error('Keep receipt outside the isolated workspace.');
  // A new session must not silently overwrite or replay an earlier execution.
  const journal = await fs.open(receipt, constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL | constants.O_NOFOLLOW, 0o600);
  const safe = (value: any) => JSON.parse(JSON.stringify(value).replaceAll(options.apiKey, '[redacted]'));
  let writes = Promise.resolve();
  const save = (event: any) => {
    const line = JSON.stringify(safe(event)) + '\n';
    writes = writes.then(async () => { await journal.writeFile(line); });
    return writes;
  };
  const provider = 'mtc_plaintext_bridge';
  const args = ['-c', `model_provider=${JSON.stringify(provider)}`, '-c', `model_providers.${provider}.name="MTC plaintext bridge"`, '-c', `model_providers.${provider}.base_url=${JSON.stringify(options.baseUrl)}`, '-c', `model_providers.${provider}.wire_api="responses"`, '-c', `model_providers.${provider}.env_key="MTC_BRIDGE_API_KEY"`];
  let server: AppServer;
  try { server = connect(options.binary, args, { ...process.env, MTC_BRIDGE_API_KEY: options.apiKey }); }
  catch (error) { await journal.close(); throw error; }
  let stopReading!: (result: IteratorResult<string>) => void;
  const stopped = new Promise<IteratorResult<string>>(resolve => { stopReading = resolve; });
  const turns = new Map<string, { id?: string; resolve: (result: TurnResult) => void; reject: (error: Error) => void; items: { id: string; text: string }[] }>();
  let disconnected: Error | undefined;
  let parentThreadId: string | undefined;
  let uncertain = false;
  let active: { id: string; task: string; calls: Map<string, { task: string; result: Promise<any> }>; children: { callId: string; childThreadId?: string; status: string }[] } | undefined;
  const ids = new Set<string>();
  const abort = () => {
    stopReading({ done: true, value: undefined });
    // Interrupt only turns belonging to this owned process. Closing it releases
    // pending RPCs even when an interrupt acknowledgement cannot be received.
    for (const [threadId, turn] of turns) {
      if (turn.id) void server.call('turn/interrupt', { threadId, turnId: turn.id }).catch(() => {});
    }
    void server.close().catch(() => {});
  };
  server.disconnected = error => {
    disconnected = error;
    stopReading({ done: true, value: undefined });
    for (const pending of turns.values()) pending.reject(error);
    turns.clear();
  };
  server.notification = message => {
    const p = message.params;
    const pending = turns.get(p?.threadId);
    if (!pending || (pending.id && p.turnId && p.turnId !== pending.id)) return;
    if (message.method === 'item/completed' && p.item?.type === 'agentMessage') pending.items.push({ id: p.item.id, text: p.item.text });
    if (message.method === 'turn/completed') {
      if (pending.id && p.turn?.id !== pending.id) return;
      turns.delete(p.threadId);
      pending.resolve({ id: p.turn.id, status: p.turn.status, error: p.turn.error, visibleMessages: pending.items });
    }
  };
  const start = async (taskId: string, model: string, tools?: unknown[]) => {
    if (uncertain || signal?.aborted || disconnected) throw disconnected ?? new Error('Bridge session ended');
    const result = await server.call('thread/start', { cwd, model, modelProvider: provider, approvalPolicy: 'never', sandbox: 'read-only', ephemeral: true, ...(tools ? { dynamicTools: tools } : {}) });
    const id = result.thread.id;
    await save({ type: 'thread', taskId, threadId: id, model, kind: tools ? 'parent' : 'child' });
    return id as string;
  };
  const turn = async (taskId: string, threadId: string, task: string, kind: string) => {
    if (signal?.aborted || disconnected) throw disconnected ?? new Error('Bridge session interrupted');
    let pending!: { id?: string; resolve: (result: TurnResult) => void; reject: (error: Error) => void; items: { id: string; text: string }[] };
    const completed = new Promise<TurnResult>((resolve, reject) => { pending = { resolve, reject, items: [] }; turns.set(threadId, pending); });
    void completed.catch(() => {});
    try {
      const started = await server.call('turn/start', { threadId, input: [{ type: 'text', text: task }] });
      pending.id = started.turn.id;
      await save({ type: 'turn', taskId, threadId, turnId: pending.id, kind });
      const result = await completed;
      if (result.id !== pending.id) throw new Error('App-server turn identity mismatch');
      await save({ type: 'result', taskId, threadId, ...result });
      return result;
    } finally { turns.delete(threadId); }
  };
  server.request = async message => {
    const p = message.params;
    const task = active;
    if (message.method !== 'item/tool/call' || p.tool !== delegateTool || p.namespace != null || typeof p.arguments?.task !== 'string' || typeof p.callId !== 'string' || typeof p.turnId !== 'string' || !task || p.threadId !== parentThreadId) throw new Error('Unsupported session delegation');
    const parentTurn = turns.get(p.threadId);
    if (!parentTurn || (parentTurn.id && parentTurn.id !== p.turnId)) throw new Error('Delegation does not belong to the active parent turn');
    const previous = task.calls.get(p.callId);
    if (previous) {
      if (previous.task !== p.arguments.task) throw new Error('Conflicting delegated call identity');
      return previous.result;
    }
    const result = (async () => {
      const child: { callId: string; childThreadId?: string; status: string } = { callId: p.callId, status: 'started' };
      task.children.push(child);
      await save({ type: 'delegation', taskId: task.id, parentThreadId, parentTurnId: p.turnId, callId: p.callId, ...(p.arguments.task === task.task ? { taskRef: task.id } : { task: p.arguments.task }) });
      try {
        child.childThreadId = await start(task.id, options.kimiModel);
        const result = await turn(task.id, child.childThreadId, p.arguments.task, 'child');
        child.status = result.status;
        await save({ type: 'call_return', taskId: task.id, callId: p.callId, childThreadId: child.childThreadId, childTurnId: result.id, status: result.status });
        return { success: result.status === 'completed', contentItems: [{ type: 'inputText', text: JSON.stringify(safe({ threadId: child.childThreadId, result })) }] };
      } catch (error) {
        child.status = 'failed'; uncertain = true;
        await save({ type: 'call_return', taskId: task.id, ...child, error: error instanceof Error ? error.message : 'child_failed', outcome: 'unknown' });
        return { success: false, contentItems: [{ type: 'inputText', text: 'Delegated task did not complete; inspect the private receipt. Do not replay an uncertain task.' }] };
      }
    })();
    task.calls.set(p.callId, { task: p.arguments.task, result });
    return result;
  };
  signal?.addEventListener('abort', abort, { once: true });
  try {
    await save({ type: 'session', mode: 'isolated-app-server-dynamic-tool-session', cwd, native_collaboration_modified: false });
    if (signal?.aborted) throw new Error('Bridge session interrupted');
    await server.call('initialize', { clientInfo: { name: 'mtc-isolated-plaintext-bridge', version: '1' }, capabilities: { experimentalApi: true } });
    server.send({ method: 'initialized', params: {} });
    while (!signal?.aborted && !disconnected) {
      const next = await Promise.race([iterator.next(), stopped]);
      if (next.done) break;
      const line = next.value;
      if (!line.trim()) continue;
      let command: any;
      try { command = JSON.parse(line); }
      catch { output({ completed: false, error: 'Expected one JSON task per line' }); continue; }
      if (typeof command.id !== 'string' || typeof command.task !== 'string' || ids.has(command.id)) {
        output(safe({ id: command.id, completed: false, error: 'Expected a new string id and plaintext task' })); continue;
      }
      ids.add(command.id);
      active = { id: command.id, task: command.task, calls: new Map(), children: [] };
      const task = active;
      const settleCalls = async () => { await Promise.allSettled([...task.calls.values()].map(call => call.result)); };
      await save({ type: 'task', id: task.id, task: task.task });
      try {
        parentThreadId ??= await start(task.id, options.parentModel, [delegateSpec]);
        const result = await turn(task.id, parentThreadId, `Use ${delegateTool} to ask Kimi to perform the following complete task, then report its actual result. Do not use collaboration spawn_agent or opaque handoff.\n\n${task.task}`, 'parent');
        // A failed parent cannot leave child callbacks writing into the next task.
        // Closing this owned process rejects pending child RPCs/turns; drain their
        // receipt writes before task_end or journal close. No uncertain replay.
        if (result.status !== 'completed') { uncertain = true; await server.close(); }
        await settleCalls();
        const completed = result.status === 'completed' && task.children.length > 0 && task.children.every(child => child.status === 'completed');
        const summary = { id: task.id, completed, parentThreadId, parentTurnId: result.id, result, delegations: task.children, receipt };
        // Results already exist once in the journal; the final event references
        // them instead of rewriting all earlier task texts and visible output.
        await save({ type: 'task_end', id: task.id, completed, parentThreadId, parentTurnId: result.id, delegations: task.children });
        output(safe(summary));
      } catch (error) {
        uncertain = true;
        await server.close();
        await settleCalls();
        const failure = safe({ id: task.id, completed: false, status: signal?.aborted ? 'interrupted' : 'failed', outcome: 'unknown', error: error instanceof Error ? error.message : 'bridge_failed', receipt });
        await save({ type: 'task_end', ...failure }); output(failure);
        break; // No second task is started after an uncertain transport outcome.
      } finally { active = undefined; }
      if (signal?.aborted || disconnected || uncertain) break;
    }
    await save({ type: 'session_end', interrupted: signal?.aborted === true, disconnected: !!disconnected, uncertain });
  } finally {
    signal?.removeEventListener('abort', abort);
    // A generic input iterator can itself be waiting on caller-owned I/O.
    // Request cleanup without awaiting that I/O after cancellation.
    const inputCleanup = iterator.return?.();
    void inputCleanup?.catch(() => {});
    try { await server.close(); await writes; }
    finally { await journal.close(); }
  }
}

async function main() {
  const { MTC_BASE_URL: baseUrl, MTC_API_KEY: apiKey, MTC_TASK_DIRECTORY: directory, MTC_TASK_ARCHIVE: receipt } = process.env;
  if (!baseUrl || !apiKey || !directory || !receipt) throw new Error('Set explicit gateway, downstream key, isolated directory and private receipt.');
  const options = { baseUrl, apiKey, directory, receipt, binary: process.env.MTC_CODEX_BINARY ?? 'codex', parentModel: process.env.MTC_PARENT_MODEL ?? 'gpt-6.1-sol', kimiModel: process.env.MTC_TASK_MODEL ?? 'k3.1-pro' };
  if (process.argv.includes('--serve')) {
    const controller = new AbortController();
    const lines = createInterface({ input: process.stdin, crlfDelay: Infinity });
    const abort = () => { controller.abort(); lines.close(); };
    process.once('SIGINT', abort); process.once('SIGTERM', abort);
    try { await serve(options, lines, result => { console.log(JSON.stringify(result)); if (!result.completed) process.exitCode = 1; }, undefined, controller.signal); }
    finally { lines.close(); process.removeListener('SIGINT', abort); process.removeListener('SIGTERM', abort); }
    return;
  }
  let task = ''; for await (const chunk of process.stdin) task += chunk;
  const result = await bridge({ ...options, task });
  console.log(JSON.stringify(result)); if (!result.completed) process.exitCode = 1;
}
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) main().catch(() => { console.error('Isolated bridge did not complete.'); process.exitCode = 1; });
