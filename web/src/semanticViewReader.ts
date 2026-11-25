import { api } from './api.js';
import type { SemanticCoverage } from './SemanticConversationViewer.js';
import type { SemanticInstanceIdentity } from './semanticConversationIdentity.js';

export interface SemanticViewInstance extends SemanticInstanceIdentity {
  atom_cursor: string;
  role?: string | null;
  origin: string;
  terminal_state: string;
  turn_id?: string | null;
}
export interface SemanticViewPage {
  status: 'pending' | 'published';
  manifest_revision: string | null;
  coverage: SemanticCoverage;
  instances: SemanticViewInstance[];
  next_cursor: string | null;
}
export interface SemanticContentField {
  name: 'text' | 'arguments' | 'arguments_text' | 'result_content' | 'tool_name' | 'call_id' | 'tool_call_id' | 'completeness' | 'kind' | 'media_type' | 'reasoning_source' | 'arguments_encoding' | 'result_status';
  format: 'text' | 'json';
  content_cursor: string;
  preview?: { text: string; byte_offset: 0; complete: boolean; next_cursor: string | null };
}
export interface SemanticPartDescriptor {
  part_index: number;
  kind: string;
  tool_name?: string;
  call_id?: string;
  completeness?: string;
  metadata_deferred?: boolean;
  fields: SemanticContentField[];
  media?: { media_type?: string; byte_length?: number; read_cursor: string };
}
export interface SemanticPartsPage {
  manifest_revision: string;
  instance_id: string;
  role: string | null;
  parts: SemanticPartDescriptor[];
  next_cursor: string | null;
}
export interface SemanticContentChunk {
  manifest_revision: string;
  instance_id: string;
  part_index: number;
  field: SemanticContentField['name'];
  format: SemanticContentField['format'];
  byte_offset: number;
  text: string;
  next_cursor: string | null;
  complete: boolean;
}
export interface SemanticViewLoader {
  page(request: string, cursor: string | undefined, signal: AbortSignal): Promise<SemanticViewPage>;
  atom(request: string, cursor: string, signal: AbortSignal): Promise<Response>;
  parts?(request: string, cursor: string, signal: AbortSignal): Promise<SemanticPartsPage>;
  content?(request: string, cursor: string, signal: AbortSignal): AsyncIterable<SemanticContentChunk>;
  media?(request: string, cursor: string, signal: AbortSignal): Promise<Response>;
}
/** Credentials remain in the owner-scoped transport, never in render props. */
export function createSemanticViewLoader(credential: string, path: (request: string) => string): SemanticViewLoader {
  const url = (request: string, kind: 'cursor' | 'atom_cursor' | 'parts_cursor' | 'content_cursor' | 'media_cursor', cursor?: string) => {
    const base = path(request);
    return cursor ? `${base}${base.includes('?') ? '&' : '?'}${kind}=${encodeURIComponent(cursor)}` : base;
  };
  return {
    async page(request, cursor, signal) {
      const value = await api<SemanticViewPage>(url(request, 'cursor', cursor), credential, { signal, cache: 'no-store' });
      if (!['pending', 'published'].includes(value.status) || !Array.isArray(value.instances) || value.instances.length > 32
        || (value.status === 'published' && (typeof value.manifest_revision !== 'string' || !/^[1-9][0-9]{0,19}$/.test(value.manifest_revision) || BigInt(value.manifest_revision) > 18446744073709551615n))
        || !value.coverage || !Array.isArray(value.coverage.gaps) || !Array.isArray(value.coverage.omissions)) throw new Error('Invalid semantic page');
      const ids = new Set<string>();
      for (const instance of value.instances) {
        if (typeof instance.instance_id !== 'string' || ids.has(instance.instance_id)
          || typeof instance.atom_cursor !== 'string' || !Number.isSafeInteger(instance.ordinal)) throw new Error('Invalid semantic instance');
        ids.add(instance.instance_id);
      }
      return value;
    },
    async parts(request, cursor, signal) {
      const value = await api<SemanticPartsPage>(url(request, 'parts_cursor', cursor), credential, { signal, cache: 'no-store' });
      if (!Array.isArray(value.parts) || typeof value.instance_id !== 'string' || typeof value.manifest_revision !== 'string'
        || !value.parts.every(part => Number.isSafeInteger(part.part_index) && part.part_index >= 0 && typeof part.kind === 'string'
          && Array.isArray(part.fields) && part.fields.every(field => ['text', 'arguments', 'arguments_text', 'result_content', 'tool_name', 'call_id', 'tool_call_id', 'completeness', 'kind', 'media_type', 'reasoning_source', 'arguments_encoding', 'result_status'].includes(field.name)
            && ['text', 'json'].includes(field.format) && typeof field.content_cursor === 'string'
            && (!field.preview || (typeof field.preview.text === 'string' && field.preview.byte_offset === 0
              && typeof field.preview.complete === 'boolean' && field.preview.complete === (field.preview.next_cursor === null)
              && (field.preview.next_cursor === null || typeof field.preview.next_cursor === 'string')))))
        || !(value.next_cursor === null || typeof value.next_cursor === 'string')) throw new Error('Invalid semantic parts');
      return value;
    },
    async *content(request, cursor, signal) {
      const response = await fetch(url(request, 'content_cursor', cursor), { signal, cache: 'no-store', headers: { Authorization: `Bearer ${credential}` } });
      if (!response.ok) throw new Error(`Semantic content HTTP ${response.status}`);
      if (!response.headers.get('content-type')?.includes('application/x-ndjson')) throw new Error('Invalid semantic content stream');
      const reader = response.body?.getReader();
      if (!reader) throw new Error('Missing semantic content stream');
      const decoder = new TextDecoder('utf-8', { fatal: true });
      let buffer = ''; let complete = false;
      try {
        while (true) {
          signal.throwIfAborted();
          const result = await reader.read();
          buffer += decoder.decode(result.value, { stream: !result.done });
          let newline: number;
          while ((newline = buffer.indexOf('\n')) >= 0) {
            const line = buffer.slice(0, newline); buffer = buffer.slice(newline + 1);
            if (!line.trim()) continue;
            if (complete) throw new Error('Unexpected semantic content after completion');
            const value = JSON.parse(line) as SemanticContentChunk;
            if (typeof value.text !== 'string' || !Number.isSafeInteger(value.byte_offset) || value.byte_offset < 0
              || typeof value.complete !== 'boolean' || value.complete !== (value.next_cursor === null)
              || !(value.next_cursor === null || typeof value.next_cursor === 'string')) throw new Error('Invalid semantic content chunk');
            complete = value.complete;
            yield value;
          }
          if (result.done) {
            if (buffer.trim() || !complete) throw new Error('Semantic content stream ended before verification');
            break;
          }
        }
      } finally { await reader.cancel(); reader.releaseLock(); }
    },
    async media(request, cursor, signal) {
      const response = await fetch(url(request, 'media_cursor', cursor), { signal, cache: 'no-store', headers: { Authorization: `Bearer ${credential}` } });
      if (!response.ok) throw new Error(`Semantic attachment HTTP ${response.status}`);
      return response;
    },
    async atom(request, cursor, signal) {
      const response = await fetch(url(request, 'atom_cursor', cursor), { signal, cache: 'no-store', headers: { Authorization: `Bearer ${credential}` } });
      if (!response.ok) throw new Error(`Semantic content HTTP ${response.status}`);
      if (!response.headers.get('content-type')?.includes('application/json')) throw new Error('Invalid semantic content');
      return response;
    },
  };
}

interface StreamFile {
  write(bytes: Uint8Array): Promise<void>;
  close(): Promise<void>;
  abort(reason?: unknown): Promise<void>;
}
type SavePicker = (options: { suggestedName: string }) => Promise<{ createWritable(): Promise<StreamFile> }>;
/** File-system stream only: no Blob, full-history array or object URL fallback. */
export async function exportSemanticContext(loader: SemanticViewLoader, request: string, signal: AbortSignal, only?: SemanticViewInstance, expectedRevision?: string): Promise<void> {
  const picker = (window as Window & { showSaveFilePicker?: SavePicker }).showSaveFilePicker;
  if (!picker) throw new Error('Streaming file export is unavailable in this browser');
  const handle = await picker.call(window, { suggestedName: `semantic-${request}.jsonl` });
  signal.throwIfAborted();
  const file = await handle.createWritable();
  const encoder = new TextEncoder();
  const writeAtom = async (instance: SemanticViewInstance, revision: string) => {
    const { atom_cursor: _cursor, ...identity } = instance;
    await file.write(encoder.encode(`${JSON.stringify({ manifest_revision: revision, instance: identity }).slice(0, -1)},"body":`));
    const response = await loader.atom(request, instance.atom_cursor, signal);
    const reader = response.body?.getReader();
    if (!reader) throw new Error('Missing semantic stream');
    try {
      while (true) {
        signal.throwIfAborted();
        const chunk = await reader.read();
        if (chunk.done) break;
        await file.write(chunk.value);
      }
    } finally { await reader.cancel(); reader.releaseLock(); }
    await file.write(encoder.encode('}\n'));
  };
  try {
    let cursor: string | undefined;
    let revision: string | undefined = expectedRevision;
    const positions: (string | undefined)[] = [];
    const visited = new Set<string>();
    let found = false;
    // Keep signed positions only; atom bodies are streamed oldest-to-newest.
    do {
      signal.throwIfAborted();
      const page = await loader.page(request, cursor, signal);
      if (page.status !== 'published' || page.manifest_revision === null) throw new Error('Semantic publication pending');
      revision ??= page.manifest_revision;
      if (page.manifest_revision !== revision) throw new Error('Semantic revision changed; export cancelled');
      positions.push(cursor);
      cursor = page.next_cursor ?? undefined;
      if (cursor && visited.has(cursor)) throw new Error('Repeated semantic page');
      if (cursor) visited.add(cursor);
    } while (cursor);
    for (const position of positions.reverse()) {
      signal.throwIfAborted();
      const page = await loader.page(request, position, signal);
      if (page.status !== 'published' || page.manifest_revision !== revision) throw new Error('Semantic revision changed; export cancelled');
      for (const instance of page.instances) {
        if (!only || (instance.instance_id === only.instance_id && instance.branch_id === only.branch_id && instance.request_id === only.request_id)) {
          await writeAtom(instance, revision!); found = true;
        }
      }
      if (only && found) break;
    }
    if (only && !found) throw new Error('Semantic instance is no longer available');
    await file.close();
  } catch (reason) { await file.abort(reason); throw reason; }
}

/** Authorized attachment bytes go directly to disk, without retaining a Blob. */
export async function exportSemanticMedia(loader: SemanticViewLoader, request: string, cursor: string, signal: AbortSignal): Promise<void> {
  const picker = (window as Window & { showSaveFilePicker?: SavePicker }).showSaveFilePicker;
  if (!picker || !loader.media) throw new Error('Streaming attachment download is unavailable in this browser');
  const handle = await picker.call(window, { suggestedName: 'attachment' });
  signal.throwIfAborted();
  const file = await handle.createWritable();
  try {
    const response = await loader.media(request, cursor, signal);
    const reader = response.body?.getReader();
    if (!reader) throw new Error('Missing attachment stream');
    try {
      while (true) {
        signal.throwIfAborted(); const chunk = await reader.read();
        if (chunk.done) break;
        await file.write(chunk.value);
      }
    } finally { await reader.cancel(); reader.releaseLock(); }
    await file.close();
  } catch (reason) { await file.abort(reason); throw reason; }
}
