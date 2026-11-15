import { api } from './api.js';
import type { SemanticCoverage } from './SemanticConversationViewer.js';
import type { SemanticInstanceIdentity } from './semanticConversationIdentity.js';

export interface SemanticViewInstance extends SemanticInstanceIdentity {
  atom_cursor: string;
  origin: string;
  terminal_state: string;
  turn_id?: string | null;
}
export interface SemanticViewPage {
  status: 'pending' | 'published';
  manifest_revision: number | null;
  coverage: SemanticCoverage;
  instances: SemanticViewInstance[];
  next_cursor: string | null;
}
export interface SemanticViewLoader {
  page(request: string, cursor: string | undefined, signal: AbortSignal): Promise<SemanticViewPage>;
  atom(request: string, cursor: string, signal: AbortSignal): Promise<Response>;
}
/** Credentials remain in the owner-scoped transport, never in render props. */
export function createSemanticViewLoader(credential: string, path: (request: string) => string): SemanticViewLoader {
  const url = (request: string, kind: 'cursor' | 'atom_cursor', cursor?: string) => {
    const base = path(request);
    return cursor ? `${base}${base.includes('?') ? '&' : '?'}${kind}=${encodeURIComponent(cursor)}` : base;
  };
  return {
    async page(request, cursor, signal) {
      const value = await api<SemanticViewPage>(url(request, 'cursor', cursor), credential, { signal, cache: 'no-store' });
      if (!['pending', 'published'].includes(value.status) || !Array.isArray(value.instances)
        || (value.status === 'published' && (!Number.isSafeInteger(value.manifest_revision) || Number(value.manifest_revision) < 1))
        || !value.coverage || !Array.isArray(value.coverage.gaps) || !Array.isArray(value.coverage.omissions)) throw new Error('Invalid semantic page');
      const ids = new Set<string>();
      for (const instance of value.instances) {
        if (typeof instance.instance_id !== 'string' || ids.has(instance.instance_id)
          || typeof instance.atom_cursor !== 'string' || !Number.isSafeInteger(instance.ordinal)) throw new Error('Invalid semantic instance');
        ids.add(instance.instance_id);
      }
      return value;
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
export async function exportSemanticContext(loader: SemanticViewLoader, request: string, signal: AbortSignal, only?: SemanticViewInstance): Promise<void> {
  const picker = (window as Window & { showSaveFilePicker?: SavePicker }).showSaveFilePicker;
  if (!picker) throw new Error('Streaming file export is unavailable in this browser');
  const handle = await picker.call(window, { suggestedName: `semantic-${request}.jsonl` });
  signal.throwIfAborted();
  const file = await handle.createWritable();
  const encoder = new TextEncoder();
  const writeAtom = async (instance: SemanticViewInstance, revision: number) => {
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
    let revision: number | undefined;
    const positions = new Set<string>();
    do {
      signal.throwIfAborted();
      const page = await loader.page(request, cursor, signal);
      if (page.status !== 'published' || page.manifest_revision === null) throw new Error('Semantic publication pending');
      revision ??= page.manifest_revision;
      if (page.manifest_revision !== revision) throw new Error('Semantic revision changed; export cancelled');
      for (const instance of page.instances) {
        if (!only || instance.instance_id === only.instance_id) await writeAtom(instance, revision);
      }
      if (only && page.instances.some(instance => instance.instance_id === only.instance_id)) break;
      cursor = page.next_cursor ?? undefined;
      if (cursor && positions.has(cursor)) throw new Error('Repeated semantic page');
      if (cursor) positions.add(cursor);
    } while (cursor);
    await file.close();
  } catch (reason) { await file.abort(reason); throw reason; }
}
