import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import { decodeTypedFilterAst } from '../src/operator/typedFilterContract.js';
import { rememberRequestFilterReload } from '../src/operator/requestFilterReload.js';
import { operatorLoginCycleStorageKey } from '../src/credentialStorage.js';

const builder = await readFile(new URL('../src/operator/TypedFilterBuilder.tsx', import.meta.url), 'utf8');
const contract = await readFile(new URL('../src/operator/typedFilterContract.ts', import.meta.url), 'utf8');
const requests = await readFile(new URL('../src/operator/pages/RequestsPage.tsx', import.meta.url), 'utf8');
const settings = await readFile(new URL('../src/operator/pages/SystemSettingsPage.tsx', import.meta.url), 'utf8');
const catalog = await readFile(new URL('../src/operator/modelCatalog.ts', import.meta.url), 'utf8');

test('a failed snapshot write removes the previous applied filter', () => {
  const original = Object.getOwnPropertyDescriptor(globalThis, 'window');
  const values = new Map<string, string>();
  let blocked = false;
  Object.defineProperty(globalThis, 'window', { configurable: true, value: {
    localStorage: { getItem: (key: string) => key === operatorLoginCycleStorageKey ? 'synthetic-cycle' : null },
    sessionStorage: {
      setItem: (key: string, value: string) => { if (blocked) throw new Error('synthetic quota exceeded'); values.set(key, value); },
      removeItem: (key: string) => { values.delete(key); },
    },
  } });
  try {
    const ast = decodeTypedFilterAst({ logical_operator: 'and', conditions: [{ field: 'model', operator: 'equals', value: { type: 'model', value: 'synthetic-model' } }] });
    assert.ok(ast);
    rememberRequestFilterReload({ cycle: 'synthetic-cycle', tenant: 'synthetic-tenant', ast });
    assert.equal(values.size, 1);
    blocked = true;
    rememberRequestFilterReload({ cycle: 'synthetic-cycle', tenant: 'synthetic-tenant', ast: {
      logical_operator: 'and', conditions: [{ field: 'model', operator: 'equals', value: { type: 'model', value: 'synthetic-replacement-model' } }],
    } });
    assert.equal(values.size, 0, 'a reload must not silently replay the old filter after a quota failure');
  } finally {
    if (original) Object.defineProperty(globalThis, 'window', original);
    else Reflect.deleteProperty(globalThis, 'window');
  }
});

test('professional request filters are schema-backed, previewed, and bound to public model routes', () => {
  assert.match(builder, /role="dialog"/);
  assert.doesNotMatch(builder, /loadFilterModelRoutes|routeModelOptions|api<ModelRouteView/);
  assert.match(builder, /groupBy="none"/);
  assert.match(builder, /filterPresetPresentation/);
  assert.doesNotMatch(builder, /\{t\('filter\.recent'\)\} \{index \+ 1\}/);
  assert.match(catalog, /valueKind === 'route' \? route\.id : route\.public_model/);
  assert.match(catalog, /route\.protocol/);
  assert.match(builder, /popover="auto"/);
  assert.doesNotMatch(builder, /aria-modal=/);
  assert.match(builder, /event\.target === event\.currentTarget && event\.newState === 'closed'/);
  assert.doesNotMatch(builder, /typed-filter-overlay|aria-modal="true"/);
  assert.doesNotMatch(builder, /\/internal\/v1\/upstream-models/);
  assert.match(builder, /setAssistantPlan/);
  assert.match(builder, /filter\.usePreview/);
  assert.match(requests, /\/internal\/v1\/requests\/query/);
  assert.match(requests, /request-technical-details/);
});

test('request and usage filters share one builder with period, credential, and granularity controls', () => {
  // The time-range quick choices and the credential alias picker are shared
  // components inside the builder, not page-specific forms injected by the
  // caller.  The builder no longer accepts an externally rendered panel.
  assert.match(builder, /FilterTimeRangeControls/);
  assert.match(builder, /FilterCredentialAliasPicker/);
  assert.doesNotMatch(builder, /panelControls|panelActive/);
  // Usage analysis sees created_at and key_alias in the same field rows.
  assert.match(contract, /\{ id: 'created_at', type: 'timestamp', usage: true \}/);
  assert.match(contract, /\{ id: 'key_alias', type: 'text', usage: true \}/);
  assert.doesNotMatch(builder, /condition\.field !== 'created_at' && condition\.field !== 'key_alias'/);
  // Draft reconciliation keeps quick-range semantics and defaults a missing
  // usage period to the last 24 hours.
  assert.match(builder, /usageSelectionFromDraft/);
  // Requests additionally offer an open-ended range; usage never does.
  assert.match(builder, /allowAllTime=\{scope === 'requests'\}/);
  // Granularity stays the single usage-only control managed inside the draft.
  assert.match(builder, /granularity=\{scope === 'usage' \? draftUsage\.granularity : undefined\}/);
  // Asynchronous reads cancel cleanly instead of racing a closed dialog.
  assert.match(builder, /new AbortController\(\)/);
});

test('reload AST decoding rejects a whole invalid filter without widening its conditions', () => {
  const range = { field: 'created_at', operator: 'between', value: { type: 'timestamp', value: 1000 }, upper: { type: 'timestamp', value: 2000 } };
  const valid = { logical_operator: 'and', conditions: [range, { field: 'model', operator: 'equals', value: { type: 'model', value: 'synthetic-model' } }] };
  assert.deepEqual(decodeTypedFilterAst(valid), valid);
  for (const invalid of [
    { ...range, field: 'unknown-field' },
    { ...range, operator: 'unknown-operator' },
    { ...range, upper: undefined },
    { ...range, upper: { type: 'text', value: '2000' } },
    { ...range, value: { type: 'timestamp', value: Infinity } },
    { field: 'status', operator: 'equals', value: { type: 'status', value: 'unknown' } },
  ]) {
    assert.equal(decodeTypedFilterAst({ logical_operator: 'and', conditions: [...valid.conditions, invalid] }), undefined);
  }
  assert.equal(decodeTypedFilterAst({ ...valid, logical_operator: 'or' }), undefined);
});

test('system filter assistant configuration persists a route reference rather than a secret', () => {
  assert.match(settings, /FilterAssistantSettings/);
  assert.match(settings, /model_route_id/);
  assert.doesNotMatch(settings, /credential_secret|api[_-]?key|client_secret/i);
});
