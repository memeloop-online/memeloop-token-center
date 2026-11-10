// Isolated, offline, single-turn experiment. This module is not a gateway adapter.
import { pathToFileURL } from 'node:url';

type Wire = Record<string, any>;
export const alias = 'mtc_plaintext_collaboration_v1';
const names = new Set(['spawn_agent', 'send_message', 'followup_task']);
export type PrototypeContext = { version: 1; alias: string; tools: string[] };
const ownsAlias = (context: PrototypeContext) => context.version === 1 && context.alias === alias && context.tools.length > 0;

export function preparePrototype(source: Wire) {
  const request = structuredClone(source);
  const addedNames = new Set<string>();
  const append = (tools: unknown) => {
    if (!Array.isArray(tools)) return;
    if (tools.some(tool => tool?.name === alias)) {
      throw new Error('Prototype alias already exists; no request was sent.');
    }
    const functions = tools
      .filter(tool => tool?.type === 'namespace' && tool.name === 'collaboration')
      .flatMap(tool => tool.tools ?? [])
      .filter(tool => tool.type === 'function' && names.has(tool.name))
      .map(tool => {
        const copy = structuredClone(tool);
        if (copy.parameters?.properties?.message) {
          delete copy.parameters.properties.message.encrypted;
        }
        return copy;
      });
    if (functions.length) {
      tools.push({ type: 'namespace', name: alias, description: 'Experimental plaintext collaboration tasks.', tools: functions });
      for (const tool of functions) addedNames.add(tool.name);
    }
  };
  append(request.tools);
  for (const item of request.input ?? []) {
    if (item.type === 'additional_tools') append(item.tools);
  }
  return { request, context: { version: 1, alias, tools: [...addedNames] } as PrototypeContext };
}

function plaintextAlias(item: Wire): boolean {
  if (item.type !== 'function_call' || item.namespace !== alias || !names.has(item.name)) return false;
  if (item.encrypted_function_args !== undefined &&
      (!Array.isArray(item.encrypted_function_args) || item.encrypted_function_args.length !== 0)) return false;
  try {
    return typeof JSON.parse(item.arguments).message === 'string';
  } catch {
    return false;
  }
}

function restoreItem(item: Wire) {
  item.namespace = 'collaboration';
  item.encrypted_function_args = [];
}

/** Only newly generated calls from this prototype's alias may become plaintext. */
export function restoreResponse(source: Wire, context: PrototypeContext) {
  const response = structuredClone(source);
  if (ownsAlias(context) && response.status === 'completed') {
    for (const item of response.output ?? []) {
      if (plaintextAlias(item) && context.tools.includes(item.name)) restoreItem(item);
    }
  }
  return response;
}

/**
 * Inspect a fully captured stream before changing any event. This intentionally
 * provides no live streaming, tool execution, history conversion or replay.
 */
export function restoreCapturedSse(source: string, context: PrototypeContext) {
  if (!ownsAlias(context)) return source;
  const frames = source.split(/(\r?\n\r?\n)/);
  const parsed = frames.map(frame => {
    const lines = frame.split(/\r?\n/);
    const data = lines.filter(line => line.startsWith('data:')).map(line => line.slice(5).trimStart()).join('\n');
    try { return JSON.parse(data) as Wire; } catch { return undefined; }
  });
  // A completed response is required as provenance for the complete arguments.
  // A failed or truncated capture passes through unchanged.
  const terminals = parsed.flatMap((event, index) => event?.type === 'response.completed' ? [{ event, index }] : []);
  if (terminals.length !== 1 || parsed.some(event =>
    ['error', 'response.failed', 'response.incomplete', 'response.cancelled'].includes(event?.type))) return source;
  const terminal = terminals[0]!;
  if (terminal.event.response?.status !== 'completed') return source;
  const terminalOutput = terminal.event.response.output;
  if (!Array.isArray(terminalOutput)) return source;
  const indexed = parsed.flatMap((event, index) => event ? [{ event, index }] : []);
  // Actual native streams may finish with output:[] after delivering complete
  // output_item.done items. Keep that empty terminal unchanged; do not fill it.
  const completedItems = terminalOutput.length ? terminalOutput : indexed
    .filter(({ event }) => event.type === 'response.output_item.done')
    .map(({ event }) => event.item);
  const calls = new Map<string, Wire>();
  const itemIds = new Set<string>();
  const callIds = new Set<string>();
  for (const item of completedItems) {
    if (typeof item.id === 'string') {
      if (itemIds.has(item.id)) return source;
      itemIds.add(item.id);
    }
    if (typeof item.call_id === 'string') {
      if (callIds.has(item.call_id)) return source;
      callIds.add(item.call_id);
    }
    if (item.namespace !== alias) continue;
    if (!plaintextAlias(item) || !context.tools.includes(item.name) ||
        typeof item.id !== 'string' || typeof item.call_id !== 'string' ||
        (!terminalOutput.length && item.status !== 'completed')) return source;
    calls.set(item.id, item);
  }
  // Every alias item event must belong to exactly one completed output call.
  // Ordinary calls may not reuse the same call ID as an alias event either.
  for (const { event, index } of indexed) {
    if (event.type !== 'response.output_item.added' && event.type !== 'response.output_item.done') continue;
    const item = event.item;
    const original = calls.get(item?.id);
    if (item?.namespace === alias && (!original || index >= terminal.index)) return source;
    if (original && (item.namespace !== alias || item.call_id !== original.call_id || item.name !== original.name)) return source;
    if (!original && [...calls.values()].some(call => call.call_id === item?.call_id)) return source;
  }
  for (const original of calls.values()) {
    const related = indexed.filter(({ event }) => event.item?.id === original.id || event.item_id === original.id);
    const added = related.filter(({ event }) => event.type === 'response.output_item.added');
    const done = related.filter(({ event }) => event.type === 'response.output_item.done');
    const argumentDone = related.filter(({ event }) => event.type === 'response.function_call_arguments.done');
    const deltas = related.filter(({ event }) => event.type === 'response.function_call_arguments.delta');
    if (added.length !== 1 || done.length !== 1 || argumentDone.length !== 1) return source;
    if (!(added[0]!.index < argumentDone[0]!.index && argumentDone[0]!.index < done[0]!.index && done[0]!.index < terminal.index)) return source;
    if (related.some(({ index }) => index >= terminal.index)) return source;
    for (const { event } of related) {
      const item = event.item ?? event;
      if ((item.encrypted_function_args !== undefined &&
          (!Array.isArray(item.encrypted_function_args) || item.encrypted_function_args.length !== 0)) ||
          (item.call_id !== undefined && item.call_id !== original.call_id) ||
          (item.namespace !== undefined && item.namespace !== alias) ||
          (item.name !== undefined && item.name !== original.name)) return source;
    }
    if (done[0]!.event.item.arguments !== original.arguments || argumentDone[0]!.event.arguments !== original.arguments) return source;
    if (deltas.some(({ event, index }) => typeof event.delta !== 'string' || index <= added[0]!.index || index >= argumentDone[0]!.index)) return source;
    const initial = added[0]!.event.item.arguments;
    if (typeof initial !== 'string' || initial + deltas.map(({ event }) => event.delta).join('') !== original.arguments) return source;
  }
  return frames.map((frame, index) => {
    const event = parsed[index];
    if (!event) return frame;
    let changed = false;
    if (event.type === 'response.output_item.added' || event.type === 'response.output_item.done') {
      const original = calls.get(event.item?.id);
      if (original && event.item.namespace === alias && event.item.call_id === original.call_id && event.item.name === original.name) {
        restoreItem(event.item);
        changed = true;
      }
    }
    if (event.type === 'response.completed') {
      for (const item of event.response?.output ?? []) {
        if (calls.has(item.id) && plaintextAlias(item)) {
          restoreItem(item);
          changed = true;
        }
      }
    }
    if (!changed) return frame;
    // Only selected item metadata changes. Argument delta/done frames, usage,
    // IDs, call IDs and parameter string values are never edited.
    const newline = frame.includes('\r\n') ? '\r\n' : '\n';
    let emitted = false;
    return frame.split(/\r?\n/).flatMap(line => {
      if (!line.startsWith('data:')) return [line];
      if (emitted) return [];
      emitted = true;
      return [`data: ${JSON.stringify(event)}`];
    }).join(newline);
  }).join('');
}

async function main() {
  let input = '';
  process.stdin.setEncoding('utf8');
  for await (const chunk of process.stdin) input += chunk;
  if (process.argv[2] === 'request') {
    console.log(JSON.stringify(preparePrototype(JSON.parse(input)), null, 2));
  } else if (process.argv[2] === 'response') {
    const { response, context } = JSON.parse(input);
    console.log(JSON.stringify(restoreResponse(response, context), null, 2));
  } else if (process.argv[2] === 'sse') {
    const { sse, context } = JSON.parse(input);
    process.stdout.write(restoreCapturedSse(sse, context));
  } else {
    throw new Error('Offline modes: request, response, sse. Request mode emits request/context; response modes require that context. No network requests or tools execute.');
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch(error => { console.error(error.message); process.exitCode = 1; });
}
