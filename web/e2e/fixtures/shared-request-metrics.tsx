import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system';
import { RequestsPage } from '../../src/operator/pages/RequestsPage';
import type { RequestView } from '../../src/types';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/styles/metrics.css';
import '../../src/styles/request-table.css';
import '../../src/operator/operator.css';

const start = Date.UTC(2026, 8, 15, 12);
const base: RequestView = { request_id: 'running', created_at: start, completed_at: null, model: 'sample-model', protocol: 'openai', status_code: null, duration_ms: null, input_tokens: 0, output_tokens: 0, cost: '0', error_code: null };
const requests: RequestView[] = [
  { ...base, input_tokens: 120, output_tokens: 30 },
  { ...base, request_id: 'success', created_at: start + 30_000, completed_at: start + 50_000, status_code: 200, duration_ms: 20_000, input_tokens: 200, output_tokens: 100, currency: 'USD', cost: '1.50' },
  { ...base, request_id: 'failure', created_at: start + 60_000, completed_at: start + 90_000, status_code: 502, duration_ms: 30_000, input_tokens: 40, output_tokens: 10, currency: 'EUR', cost: '0.25' },
];
window.fetch = async input => {
  const path = new URL(String(input), location.origin).pathname;
  if (path !== '/internal/v1/requests/query' && path !== '/internal/v1/upstreams') throw new Error(`Unexpected fixture request: ${path}`);
  return new Response(JSON.stringify(path.endsWith('/query') ? { requests, next_cursor: null } : []), { headers: { 'Content-Type': 'application/json' } });
};
createRoot(document.getElementById('root')!).render(<I18nProvider><MtcFluentProvider><main className="main"><RequestsPage token="fixture" tenant="fixture" liveEvents={new Map()} streamRevision={0} streamState="live" streamError="" onOpenSessions={() => {}} onOpenSession={() => {}} /></main></MtcFluentProvider></I18nProvider>);
