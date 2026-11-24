import { useState } from 'react';
import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system';
import { SemanticSessionPanel } from '../../src/SemanticSessionPanel';
import type { LogicalSessionDetail } from '../../src/types';
import { createSemanticViewLoader, type SemanticContentChunk, type SemanticViewLoader } from '../../src/semanticViewReader';
import '../../src/styles.css';
import '../../src/theme.css';

declare global { interface Window { semanticAtomReads: number; semanticPageReads: number } }
window.semanticAtomReads = 0; window.semanticPageReads = 0;
declare global { interface Window { semanticPageRequest: string; semanticContentReads: number } }
window.semanticPageRequest = ''; window.semanticContentReads = 0;
const detail: LogicalSessionDetail = {
  session_id: 'session-a', cluster_id: null, unlinked: false, edges: [], edges_truncated: false, has_more: false, next_cursor: null,
  requests: [{ request_id: 'request-a', created_at: 1, protocol: 'openai', model: 'fixture', status_code: 200, duration_ms: 1,
    input_tokens: 1, output_tokens: 1, cost: '0', error_code: null, source: 'live', provenance: 'native', unlinked: false }],
};
const revision = '18446744073709551615';
const largeChunk = '连续内容😀'.repeat(4096);
const scenario = new URLSearchParams(location.search).get('scenario');
async function* contentFrames(_request: string, cursor: string): AsyncGenerator<SemanticContentChunk> {
    const [instance, field, indexText] = cursor.split(':');
    const index = Number(indexText ?? 0);
    if (scenario === 'large' && (field === 'text' || field === 'arguments' || field === 'result_content')) {
      for (let next = index; next <= 5; next++) {
        yield { manifest_revision: revision, instance_id: instance, part_index: field === 'text' ? 0 : field === 'arguments' ? 2 : 3, field, format: field === 'text' ? 'text' : 'json', byte_offset: next * new TextEncoder().encode(largeChunk).byteLength, text: next === 5 ? 'COMPLETE LARGE FIELD' : largeChunk, next_cursor: next === 5 ? null : `${instance}:${field}:${next + 1}`, complete: next === 5 };
      } return;
    }
    if (scenario === 'preview' || scenario === 'preview-partial') { yield { manifest_revision: revision, instance_id: instance, part_index: 0, field: 'text', format: 'text', byte_offset: 10, text: ' tail after preview', next_cursor: null, complete: true }; return; }
    if (scenario === 'revision') { yield { manifest_revision: '2', instance_id: instance, part_index: 0, field: 'text', format: 'text', byte_offset: 0, text: 'Stale content', next_cursor: null, complete: true }; return; }
    yield { manifest_revision: '18446744073709551615', instance_id: instance, part_index: field === 'text' ? 0 : field === 'reasoning' ? 1 : 2,
      field: field === 'arguments' ? 'arguments' : 'text', format: field === 'arguments' ? 'json' : 'text', byte_offset: 0,
      text: field === 'text' ? instance.startsWith('item-') ? `Message ${instance}` : `Verified ${instance === 'latest' ? 'latest' : 'earlier'} answer` : field === 'reasoning' ? 'Separate reasoning' : '{"query": "structured"}',
      next_cursor: null, complete: true };
}

const originalFetch = window.fetch.bind(window);
window.fetch = async (input, init) => {
  const url = new URL(String(input), location.origin);
  if (url.pathname !== '/fixture-content') return originalFetch(input, init);
  window.semanticContentReads++;
  const frames = contentFrames('request-a', url.searchParams.get('content_cursor')!);
  let pending = new Uint8Array();
  return new Response(new ReadableStream<Uint8Array>({
    async pull(controller) {
      if (!pending.length) {
        const result = await frames.next();
        if (result.done) { controller.close(); return; }
        pending = new TextEncoder().encode(JSON.stringify(result.value) + '\n');
      }
      controller.enqueue(pending.slice(0, 8191)); pending = pending.slice(8191);
    },
    async cancel() { await frames.return(undefined); },
  }), { headers: { 'content-type': 'application/x-ndjson' } });
};
const contentLoader = createSemanticViewLoader('fixture', () => '/fixture-content');
const loader: SemanticViewLoader = {
  page: async (_request, cursor) => {
    window.semanticPageReads++; window.semanticPageRequest = _request;
    if (scenario === 'error') throw new Error('Semantic read HTTP 500');
    if (scenario === 'many') return { status: 'published', manifest_revision: revision, coverage: { observed_terminal: true, projection_state: 'complete', gaps: [], omissions: [] },
      instances: Array.from({ length: cursor ? 1 : 32 }, (_, i) => ({ instance_id: `item-${cursor ? 0 : i + 1}`, request_id: 'request-a', branch_id: 'branch-a', ordinal: cursor ? 0 : i + 1, role: 'assistant', atom_hash: 'same', node_hash: 'same', atom_cursor: `item-${cursor ? 0 : i + 1}`, origin: 'return', terminal_state: 'complete' })), next_cursor: cursor ? null : 'older-page' };
    return { status: 'published', manifest_revision: '18446744073709551615',
      coverage: { observed_terminal: true, projection_state: 'partial', gaps: ['tool_result_not_recorded'], omissions: [] },
      instances: [{ instance_id: cursor ? 'earlier' : 'latest', request_id: 'request-a', branch_id: 'branch-a', ordinal: cursor ? 1 : 2,
        role: scenario === 'unknown' ? null : cursor ? 'user' : 'assistant', atom_hash: 'a'.repeat(64), node_hash: 'b'.repeat(64), atom_cursor: cursor ?? 'latest-atom', origin: 'return', terminal_state: 'complete' }],
      next_cursor: cursor ? null : 'older-page' };
  },
  parts: async (_request, cursor) => {
    window.semanticAtomReads++;
    const instance = cursor.startsWith('item-') ? cursor : cursor === 'latest-atom' ? 'latest' : 'earlier';
    return { manifest_revision: '18446744073709551615', instance_id: instance, role: 'assistant', next_cursor: null,
      parts: [
        { part_index: 0, kind: 'text', fields: [{ name: 'text', format: 'text', content_cursor: `${instance}:text`, ...(scenario?.startsWith('preview') ? { preview: { text: '😀首窗', byte_offset: 0 as const, complete: scenario === 'preview', next_cursor: scenario === 'preview' ? null : `${instance}:text:1` } } : {}) }] },
        { part_index: 1, kind: 'reasoning', fields: [{ name: 'text', format: 'text', content_cursor: `${instance}:reasoning` }] },
        { part_index: 2, kind: 'tool_call', tool_name: 'lookup', call_id: 'call-1', fields: [{ name: 'arguments', format: 'json', content_cursor: `${instance}:arguments` }] },
        ...(scenario === 'large' ? [{ part_index: 3, kind: 'tool_result', tool_name: 'lookup', call_id: 'call-1', fields: [{ name: 'result_content' as const, format: 'json' as const, content_cursor: `${instance}:result_content` }] }] : []),
      ] };
  },
  content: (request, cursor, signal) => contentLoader.content!(request, cursor, signal),
  atom: async (_request, cursor) => {
    window.semanticAtomReads++;
    return new Response(JSON.stringify({ role: scenario === 'unknown' ? 'unrecorded-provider-role' : cursor === 'latest-atom' ? 'assistant' : 'user', parts: [
      { kind: 'text', text: cursor === 'latest-atom' ? 'Verified latest answer' : 'Verified earlier answer' },
      { kind: 'reasoning', text: 'Separate reasoning' },
      { kind: 'tool_call', tool_name: 'lookup', arguments: { query: 'structured' }, call_id: 'call-1' },
    ] }));
  },
};
function SessionFixture() {
  const [other, setOther] = useState(false);
  const selected = other ? { ...detail, session_id: 'session-b', requests: detail.requests.map(request => ({ ...request, request_id: 'request-b' })) } : detail;
  return <>{scenario === 'scope' && <button onClick={() => setOther(true)}>Open another session</button>}<SemanticSessionPanel detail={selected} loader={loader} /></>;
}
createRoot(document.getElementById('root')!).render(<I18nProvider><MtcFluentProvider><SessionFixture /></MtcFluentProvider></I18nProvider>);
