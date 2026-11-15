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
