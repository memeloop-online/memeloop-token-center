import { createRoot } from 'react-dom/client';
import { useState } from 'react';
import { I18nProvider } from '../../src/i18n';
import { TypedFilterBuilder } from '../../src/operator/TypedFilterBuilder';
import type { FilterPresetState, ModelRouteView, TypedFilterAst } from '../../src/types';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/operator.css';

const initial: TypedFilterAst = { logical_operator: 'and', conditions: [{ field: 'model', operator: 'equals', value: { type: 'model', value: '' } }] };
const recent: TypedFilterAst = { logical_operator: 'and', conditions: [
  { field: 'created_at', operator: 'between', value: { type: 'timestamp', value: Date.UTC(2026, 9, 15) }, upper: { type: 'timestamp', value: Date.UTC(2026, 9, 16) } },
  { field: 'model', operator: 'equals', value: { type: 'model', value: 'gpt-6-astra' } },
  { field: 'status', operator: 'equals', value: { type: 'status', value: 'error' } },
] };
const routes: ModelRouteView[] = Array.from({ length: 101 }, (_, index) => ({
  id: `route-${index}`, public_model: index === 100 ? 'gpt-6-astra' : `model-${index}`, upstream_model: 'native-model', protocol: 'openai', priority: 0, enabled: false, created_at: 200 - index, updated_at: 0, grant_revision: 0,
}));
let presets: FilterPresetState = { named: [{ name: 'Saved '.repeat(40).trim(), ast: recent }], recent: [recent] };
const catalogError = new URLSearchParams(location.search).has('catalogError');
// Synthetic credential; every fetch is answered here and no request reaches an integration.
globalThis.fetch = async (input, init) => {
  const url = new URL(String(input), location.origin);
  const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status });
  if (url.pathname === '/internal/v1/model-routes') {
    if (catalogError) return json({}, 503);
    const cursor = url.searchParams.get('before_id');
    const start = cursor ? routes.findIndex((route) => route.id === cursor) + 1 : 0;
    return json(routes.slice(start, start + Number(url.searchParams.get('limit') ?? 100)));
  }
  if (url.pathname === '/internal/v1/provider-groups') return json({}, 503);
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
