import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system';
import { SemanticSessionPanel } from '../../src/SemanticSessionPanel';
import type { LogicalSessionDetail } from '../../src/types';
import type { SemanticViewLoader } from '../../src/semanticViewReader';
import '../../src/styles.css';
import '../../src/theme.css';

declare global { interface Window { semanticAtomReads: number; semanticPageReads: number } }
window.semanticAtomReads = 0; window.semanticPageReads = 0;
const detail: LogicalSessionDetail = {
  session_id: 'session-a', cluster_id: null, unlinked: false, edges: [], edges_truncated: false, has_more: false, next_cursor: null,
  requests: [{ request_id: 'request-a', created_at: 1, protocol: 'openai', model: 'fixture', status_code: 200, duration_ms: 1,
    input_tokens: 1, output_tokens: 1, cost: '0', error_code: null, source: 'live', provenance: 'native', unlinked: false }],
};
const loader: SemanticViewLoader = {
  page: async (_request, cursor) => {
    window.semanticPageReads++;
    return { status: 'published', manifest_revision: '18446744073709551615',
      coverage: { observed_terminal: true, projection_state: 'partial', gaps: ['tool_result_not_recorded'], omissions: [] },
      instances: [{ instance_id: cursor ? 'earlier' : 'latest', request_id: 'request-a', branch_id: 'branch-a', ordinal: cursor ? 1 : 2,
        role: cursor ? 'user' : 'assistant', atom_hash: 'a'.repeat(64), node_hash: 'b'.repeat(64), atom_cursor: cursor ?? 'latest-atom', origin: 'return', terminal_state: 'complete' }],
      next_cursor: cursor ? null : 'older-page' };
  },
  atom: async (_request, cursor) => {
    window.semanticAtomReads++;
    return new Response(JSON.stringify({ role: cursor === 'latest-atom' ? 'assistant' : 'user', parts: [
      { kind: 'text', text: cursor === 'latest-atom' ? 'Verified latest answer' : 'Verified earlier answer' },
      { kind: 'reasoning', text: 'Separate reasoning' },
      { kind: 'tool_call', tool_name: 'lookup', arguments: { query: 'structured' }, call_id: 'call-1' },
    ] }));
  },
};
createRoot(document.getElementById('root')!).render(<I18nProvider><MtcFluentProvider><SemanticSessionPanel detail={detail} loader={loader} /></MtcFluentProvider></I18nProvider>);
