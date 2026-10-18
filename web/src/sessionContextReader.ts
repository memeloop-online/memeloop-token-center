import { apiRead } from './api.js';

export interface SessionContextItem {
  node_id: string;
  depth: number;
  role: string;
  kind: string;
  phase: string | null;
  text: string | null;
  text_complete: boolean;
  source_chars: number | null;
  content_cursor: string;
  text_cursor?: string | null;
}

export interface SessionContextPage {
  contract_version: 'session_context_v1';
  representation: 'semantic_context';
  request_id: string;
  session_id: string;
  items: SessionContextItem[];
  next_cursor: string | null;
}

export interface SessionContextChunk {
  contract_version: 'session_context_content_v1';
  representation: 'semantic_context' | 'message_text';
  node_id: string;
  offset_chars: number;
  total_chars: number;
  content: string;
  next_cursor: string | null;
}

export interface SessionContextLoader {
  page: (requestId: string, cursor: string | undefined, details: boolean, signal: AbortSignal) => Promise<SessionContextPage>;
  content: (requestId: string, cursor: string, signal: AbortSignal) => Promise<SessionContextChunk>;
}

type JsonObject = Record<string, unknown>;
function record(value: unknown): value is JsonObject { return !!value && typeof value === 'object' && !Array.isArray(value); }
function integer(value: unknown): value is number { return Number.isSafeInteger(value) && Number(value) >= 0; }
function cursor(value: unknown): value is string | null { return value === null || typeof value === 'string' && value.length > 0 && value.length <= 4096; }
function hash(value: unknown): value is string { return typeof value === 'string' && /^[a-f0-9]{64}$/.test(value); }
function invalid(): Error { return new Error('Invalid session context response'); }

/** Validate wire identity before admitting any result to a shared reading view. */
export function validateContextPage(value: unknown, sessionId: string, requestId: string): SessionContextPage {
  if (!record(value) || value.contract_version !== 'session_context_v1' || value.representation !== 'semantic_context'
    || value.session_id !== sessionId || value.request_id !== requestId || !cursor(value.next_cursor)
    || !Array.isArray(value.items) || value.items.length > 32) throw invalid();
  let previousDepth: number | undefined;
  const identities = new Set<string>();
  for (const item of value.items) {
    if (!record(item) || !hash(item.node_id) || !integer(item.depth) || item.depth < 1
      || typeof item.role !== 'string' || typeof item.kind !== 'string'
      || !(item.phase === null || typeof item.phase === 'string')
      || !(item.text === null || typeof item.text === 'string')
      || typeof item.text_complete !== 'boolean'
      || !(item.source_chars === null || integer(item.source_chars))
      || !cursor(item.content_cursor) || item.content_cursor === null
      || item.text_cursor !== undefined && !cursor(item.text_cursor)
      || previousDepth !== undefined && item.depth !== previousDepth + 1
      || identities.has(item.node_id)) throw invalid();
    identities.add(item.node_id);
    previousDepth = item.depth;
  }
  if (value.items.length === 0 && value.next_cursor !== null) throw invalid();
  return value as unknown as SessionContextPage;
}

export function validateContextChunk(value: unknown): SessionContextChunk {
  if (!record(value) || value.contract_version !== 'session_context_content_v1'
    || !['semantic_context', 'message_text'].includes(String(value.representation)) || !hash(value.node_id)
    || !integer(value.offset_chars) || !integer(value.total_chars) || typeof value.content !== 'string'
    || !cursor(value.next_cursor)) throw invalid();
  const length = Array.from(value.content).length;
  const end = value.offset_chars + length;
  if (length > 16_384 || !Number.isSafeInteger(end) || end > value.total_chars
    || (end < value.total_chars) !== (value.next_cursor !== null)
    || end < value.total_chars && length === 0) throw invalid();
  return value as unknown as SessionContextChunk;
}

/** The caller provides its authorized route. No credentials enter display components. */
export function createSessionContextLoader(sessionId: string, credential: string, path: (requestId: string, content: boolean) => string): SessionContextLoader {
  let active = 0;
  const waiting: Array<() => void> = [];
  const bounded = async <T>(signal: AbortSignal, read: () => Promise<T>): Promise<T> => {
    while (active >= 4) {
      await new Promise<void>((resolve, reject) => {
        const ready = () => { signal.removeEventListener('abort', aborted); resolve(); };
        const aborted = () => {
          const index = waiting.indexOf(ready);
          if (index >= 0) waiting.splice(index, 1);
          reject(signal.reason);
        };
        if (signal.aborted) { reject(signal.reason); return; }
        waiting.push(ready);
        signal.addEventListener('abort', aborted, { once: true });
      });
    }
    signal.throwIfAborted();
    active += 1;
    try { return await read(); }
    finally { active -= 1; waiting.shift()?.(); }
  };
  const queryPath = (base: string, values: Record<string, string>) => {
    const separator = base.includes('?') ? '&' : '?';
    return `${base}${separator}${new URLSearchParams(values)}`;
  };
  return {
    async page(requestId, continuation, details, signal) {
      const response = await apiRead<unknown>(queryPath(path(requestId, false), {
        ...(continuation ? { cursor: continuation } : {}), details: String(details),
      }), credential, { signal, totalTimeoutMilliseconds: 15_000 });
      return validateContextPage(response, sessionId, requestId);
    },
    async content(requestId, continuation, signal) {
      return bounded(signal, async () => validateContextChunk(await apiRead<unknown>(
        queryPath(path(requestId, true), { cursor: continuation }), credential,
        { signal, totalTimeoutMilliseconds: 15_000 },
      )));
    },
  };
}

/**
 * One invocation reads exactly one page. No prefetch or recursive draining.
 * Keep each request's path separate: resets/branches cannot overwrite history.
 */
export function prependContextPage(existing: SessionContextPage | undefined, older: SessionContextPage): SessionContextPage {
  if (!existing) return older;
  if (existing.request_id !== older.request_id || existing.session_id !== older.session_id
    || existing.next_cursor === null || older.items.length === 0
    || older.items.at(-1)!.depth + 1 !== existing.items[0]?.depth) throw invalid();
  const identities = new Set(existing.items.map((item) => item.node_id));
  if (older.items.some((item) => identities.has(item.node_id))) throw invalid();
  return { ...older, items: [...older.items, ...existing.items] };
}

/** Used only after a content disclosure is opened. Unicode offsets are scalars, not JS UTF-16 indices. */
export interface ContextContentState {
  nodeId: string;
  totalChars: number;
  loadedChars: number;
  chunks: string[];
  nextCursor: string | null;
  representation: SessionContextChunk['representation'];
}

export function appendContextChunk(previous: ContextContentState | undefined, nodeId: string, chunk: SessionContextChunk): ContextContentState {
  if (chunk.node_id !== nodeId || chunk.offset_chars !== (previous?.loadedChars ?? 0)
    || previous && (previous.nodeId !== nodeId || previous.totalChars !== chunk.total_chars || previous.representation !== chunk.representation || previous.nextCursor === null)) throw invalid();
  return {
    nodeId,
    totalChars: chunk.total_chars,
    loadedChars: chunk.offset_chars + Array.from(chunk.content).length,
    chunks: [...previous?.chunks ?? [], chunk.content],
    nextCursor: chunk.next_cursor,
    representation: chunk.representation,
  };
}

/** Only protocol-declared text is a user message; never stringify embedded tool or media blocks. */
export function contextMessageText(value: unknown): string | undefined {
  if (!record(value)) return undefined;
  if (typeof value.content === 'string') return value.content;
  if (!Array.isArray(value.content)) return undefined;
  const text = value.content.flatMap((part) => record(part)
    && ['text', 'input_text', 'output_text'].includes(String(part.type)) && typeof part.text === 'string'
    ? [part.text] : []);
  return text.length ? text.join('\n') : undefined;
}
