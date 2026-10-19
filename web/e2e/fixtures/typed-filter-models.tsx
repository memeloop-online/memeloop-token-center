import { createRoot } from 'react-dom/client';
import { useState } from 'react';
import { I18nProvider } from '../../src/i18n';
import { TypedFilterBuilder } from '../../src/operator/TypedFilterBuilder';
import type { FilterPresetState, TypedFilterAst } from '../../src/types';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/operator.css';

const initial: TypedFilterAst = { logical_operator: 'and', conditions: [{ field: 'model', operator: 'equals', value: { type: 'model', value: '' } }] };
const recent: TypedFilterAst = { logical_operator: 'and', conditions: [
  { field: 'created_at', operator: 'between', value: { type: 'timestamp', value: Date.UTC(2026, 9, 15) }, upper: { type: 'timestamp', value: Date.UTC(2026, 9, 16) } },
  { field: 'model', operator: 'equals', value: { type: 'model', value: 'gpt-6-astra' } },
  { field: 'status', operator: 'equals', value: { type: 'status', value: 'error' } },
] };
const names = [...Array.from({ length: 101 }, (_, index) => `model-${index}`), 'gpt-6-astra'];
const catalogRequests: string[] = [];
let presets: FilterPresetState = { named: [{ name: 'Saved '.repeat(40).trim(), ast: recent }], recent: [recent] };
const catalogError = new URLSearchParams(location.search).has('catalogError');
// Synthetic credential; every fetch is answered here and no request reaches an integration.
globalThis.fetch = async (input, init) => {
  const url = new URL(String(input), location.origin);
  const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status });
  if (url.pathname === '/internal/v1/filter-model-names') {
    const search = url.searchParams.get('search') ?? '';
    catalogRequests.push(search);
    document.body.dataset.catalogRequests = JSON.stringify(catalogRequests);
    if (catalogError) return json({}, 503);
    // Ignore abort deliberately: stale responses must still not replace current suggestions.
    if (search === 'slow') {
      await new Promise((resolve) => setTimeout(resolve, 700));
      return json(['slow-historical-model']);
    }
    return json(names.filter((name) => name.toLowerCase().includes(search.toLowerCase())).slice(0, Number(url.searchParams.get('limit') ?? 50)));
  }
  if (url.pathname === '/internal/v1/model-routes' || url.pathname === '/internal/v1/provider-groups') throw new Error('Filters must use name-only suggestions');
  if (url.pathname === '/internal/v1/filter-presets') {
    if (init?.method === 'POST') {
      const body = JSON.parse(String(init.body));
      presets = { ...presets, recent: [body.ast, ...presets.recent].slice(0, 8) };
    }
    return json(presets);
  }
  return json(null);
};

function Fixture() {
  const [ast, setAst] = useState(initial);
  return <main style={{ padding: 8 }}>
    <TypedFilterBuilder ast={ast} onApply={setAst} onClear={() => setAst(initial)} scope="requests" token="synthetic-filter-token" tenant="fixture" upstreams={[]} />
    <output id="applied-filter">{JSON.stringify(ast)}</output>
  </main>;
}
createRoot(document.getElementById('root')!).render(<I18nProvider><Fixture /></I18nProvider>);
