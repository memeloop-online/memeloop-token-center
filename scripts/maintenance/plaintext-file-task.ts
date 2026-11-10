import fs from 'node:fs/promises';
import { constants } from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

type ToolCall = { id: string; type: 'function'; function: { name: string; arguments: string } };
type Message = { role: string; content: string | null; tool_calls?: ToolCall[]; tool_call_id?: string; reasoning_content?: string };
type Options = { baseUrl: string; apiKey: string; model: string; task: string; directory: string; archive: string };

export const tools = [
  ['list_files', 'List names and types in a workspace directory.', { path: { type: 'string' } }, ['path']],
  ['read_file', 'Read a complete UTF-8 workspace file.', { path: { type: 'string' } }, ['path']],
  ['write_file', 'Replace a UTF-8 workspace file with complete content. Parent directory must exist.', { path: { type: 'string' }, content: { type: 'string' } }, ['path', 'content']],
].map(([name, description, properties, required]) => ({ type: 'function', function: {
  name, description, parameters: { type: 'object', properties, required, additionalProperties: false },
} }));

/** Only file tools: no shell, credential access, network tools or hidden task envelope. */
export async function fileTool(directory: string, name: string, args: Record<string, unknown>) {
  const root = await fs.realpath(directory);
  if (typeof args.path !== 'string' || path.isAbsolute(args.path)) throw new Error('Use a relative workspace path.');
  const target = path.resolve(root, args.path);
  const inside = (p: string) => p === root || p.startsWith(root + path.sep);
  if (!inside(target)) throw new Error('Path is outside the task workspace.');
  if (name === 'write_file') {
    if (typeof args.content !== 'string') throw new Error('content must be a string.');
    if (!inside(await fs.realpath(path.dirname(target)))) throw new Error('Parent resolves outside workspace.');
    // Existing symlinks cannot turn a workspace edit into an external write.
    const existing = await fs.lstat(target).catch((error: NodeJS.ErrnoException) => {
      if (error.code !== 'ENOENT') throw error;
      return undefined;
    });
    if (existing?.isSymbolicLink()) throw new Error('Write destination is a symlink.');
    await fs.writeFile(target, args.content, { encoding: 'utf8', flag: 'w' });
    return { path: args.path, bytes: Buffer.byteLength(args.content) };
  }
  if (!inside(await fs.realpath(target))) throw new Error('Path resolves outside workspace.');
  if (name === 'read_file') return { path: args.path, content: await fs.readFile(target, 'utf8') };
  if (name === 'list_files') return { path: args.path, entries: (await fs.readdir(target, { withFileTypes: true })).map(e => ({ name: e.name, type: e.isDirectory() ? 'directory' : e.isFile() ? 'file' : 'other' })) };
  throw new Error('Unknown file tool.');
}

export async function chatTurn(response: Response) {
  const assistant: Message = { role: 'assistant', content: '' };
  const calls = new Map<number, ToolCall>();
  let model: string | undefined, usage: unknown, finish: string | undefined;
  let done = false, error: unknown;
  const consume = (chunk: any) => {
    if (chunk.error) error = chunk.error;
    model = chunk.model ?? model;
    usage = chunk.usage ?? usage;
    const choice = chunk.choices?.[0];
    finish = choice?.finish_reason ?? finish;
    const delta = choice?.delta;
    if (!delta) return;
    if (typeof delta.content === 'string') assistant.content += delta.content;
    // Some thinking providers require this on the following tool-result turn.
    // Keep it only in memory for protocol replay; never print or archive reasoning.
    if (typeof delta.reasoning_content === 'string') assistant.reasoning_content = (assistant.reasoning_content ?? '') + delta.reasoning_content;
    for (const part of delta.tool_calls ?? []) {
      if (!Number.isInteger(part.index)) throw new Error('Missing tool-call index.');
      const call = calls.get(part.index) ?? { id: '', type: 'function', function: { name: '', arguments: '' } };
      if (part.id) call.id += part.id;
      if (part.function?.name) call.function.name += part.function.name;
      if (part.function?.arguments) call.function.arguments += part.function.arguments;
      calls.set(part.index, call);
    }
  };
  if (response.headers.get('content-type')?.includes('text/event-stream')) {
    let buffer = '';
    const decoder = new TextDecoder();
    const drain = (last = false) => {
      let boundary;
      const frame = (text: string) => {
        const data = text.split(/\r?\n/).filter(l => l.startsWith('data:')).map(l => l.slice(5).replace(/^ /, '')).join('\n');
        if (!data) return;
        if (done) throw new Error('Data after stream terminal.');
        if (data === '[DONE]') done = true;
        else consume(JSON.parse(data));
      };
      while ((boundary = /\r?\n\r?\n/.exec(buffer))) {
        frame(buffer.slice(0, boundary.index));
        buffer = buffer.slice(boundary.index + boundary[0].length);
      }
      if (last && buffer.trim()) { frame(buffer); buffer = ''; }
    };
    try {
      if (response.body) for await (const bytes of response.body) { buffer += decoder.decode(bytes, { stream: true }); drain(); }
      buffer += decoder.decode(); drain(true);
    } catch { error = { type: 'incomplete_or_invalid_stream' }; }
    assistant.tool_calls = [...calls.entries()].sort(([a], [b]) => a - b).map(([, call]) => call);
  } else {
    try {
      const data = await response.json();
      error = data.error; model = data.model; usage = data.usage;
      Object.assign(assistant, data.choices?.[0]?.message ?? {});
      finish = data.choices?.[0]?.finish_reason;
      done = !!data.choices?.[0]?.message;
    } catch { error = { type: 'non_json_response' }; }
  }
  const ids = new Set<string>();
  for (const call of assistant.tool_calls ?? []) {
    if (!call.id || ids.has(call.id) || call.type !== 'function' || !call.function?.name || typeof call.function.arguments !== 'string') error = { type: 'invalid_tool_call_identity' };
    ids.add(call.id);
  }
  return { status: response.status, request_id: response.headers.get('x-mtc-request-id') ?? response.headers.get('x-request-id'), model, usage, finish_reason: finish, stream_complete: done && !error, error, assistant };
}

export async function runTask(options: Options, send: typeof fetch = fetch) {
  const directory = await fs.realpath(options.directory);
  const archive = path.resolve(options.archive);
  const archiveParent = await fs.realpath(path.dirname(archive));
  if (archiveParent === directory || archiveParent.startsWith(directory + path.sep)) throw new Error('Keep the task receipt outside the model workspace.');
  const messages: Message[] = [
    { role: 'system', content: 'Work on the supplied task using the workspace file tools. Read files before editing them. Use complete plaintext tool arguments. Do not claim tests were run: no execution tool is available. Return a final useful result after any necessary file operations.' },
    { role: 'user', content: options.task },
  ];
  const record: any = { model: options.model, directory, task: options.task, turns: [], completed: false };
  const redact = (value: unknown) => JSON.parse(JSON.stringify(value).replaceAll(options.apiKey, '[redacted]'));
  const save = async () => {
    // Do not follow a final-component symlink back into the model workspace.
    const file = await fs.open(archive, constants.O_WRONLY | constants.O_CREAT | constants.O_TRUNC | constants.O_NOFOLLOW, 0o600);
    try { await file.chmod(0o600); await file.writeFile(JSON.stringify(redact(record), null, 2) + '\n'); }
    finally { await file.close(); }
  };
  await save();
  while (true) {
    let turn;
    try {
      turn = await chatTurn(await send(`${options.baseUrl.replace(/\/$/, '')}/chat/completions`, {
        method: 'POST', headers: { Authorization: `Bearer ${options.apiKey}`, 'Content-Type': 'application/json', 'X-Mtc-Client-Name': 'plaintext-file-task' },
        body: JSON.stringify({ model: options.model, messages, tools, stream: true, stream_options: { include_usage: true } }),
      }));
    } catch { record.error = { type: 'transport_error', outcome: 'unknown' }; await save(); return redact(record); }
    const { reasoning_content: _reasoning, ...visible } = turn.assistant;
    record.turns.push({ ...turn, assistant: visible });
    await save();
    if (turn.status < 200 || turn.status >= 300 || !turn.stream_complete || turn.error) return redact(record);
    const pending = turn.assistant.tool_calls ?? [];
    if (!pending.length) {
      record.completed = turn.finish_reason === 'stop';
      record.result = turn.assistant.content;
      await save(); return redact(record);
    }
    if (turn.finish_reason !== 'tool_calls') { record.error = { type: 'unfinished_tool_calls' }; await save(); return redact(record); }
    messages.push(turn.assistant);
    for (const call of pending) {
      let result;
      try { result = { ok: true, result: await fileTool(directory, call.function.name, JSON.parse(call.function.arguments)) }; }
      catch (error) { result = { ok: false, error: error instanceof Error ? error.message : 'file_tool_failed' }; }
      const content = JSON.stringify(redact(result));
      record.turns.at(-1).tool_results ??= [];
      record.turns.at(-1).tool_results.push({ tool_call_id: call.id, content });
      await save();
      messages.push({ role: 'tool', tool_call_id: call.id, content });
    }
  }
}

async function main() {
  if (process.argv.includes('--help')) { console.log('Read task from stdin. Set MTC_API_KEY, MTC_TASK_DIRECTORY (isolated workspace), MTC_TASK_ARCHIVE (private JSON), optionally MTC_TASK_MODEL and MTC_BASE_URL. File tools only; no automatic retry, output cap or total deadline.'); return; }
  const apiKey = process.env.MTC_API_KEY, directory = process.env.MTC_TASK_DIRECTORY, archive = process.env.MTC_TASK_ARCHIVE;
  if (!apiKey || !directory || !archive) throw new Error('Set key, isolated directory and private archive path.');
  let task = ''; process.stdin.setEncoding('utf8'); for await (const chunk of process.stdin) task += chunk;
  const result = await runTask({ apiKey, directory, archive, task, model: process.env.MTC_TASK_MODEL ?? 'k3.1-pro', baseUrl: process.env.MTC_BASE_URL ?? 'https://token.k3s.onetwo.website/v1' });
  console.log(JSON.stringify(result, null, 2));
  if (!result.completed) process.exitCode = 1;
}
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) main().catch(() => { console.error('Task runner could not complete; no automatic retry was made.'); process.exitCode = 1; });
