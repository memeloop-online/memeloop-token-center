import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const builder = await readFile(new URL('../src/operator/TypedFilterBuilder.tsx', import.meta.url), 'utf8');
const requests = await readFile(new URL('../src/operator/pages/RequestsPage.tsx', import.meta.url), 'utf8');
const settings = await readFile(new URL('../src/operator/pages/SystemSettingsPage.tsx', import.meta.url), 'utf8');
const catalog = await readFile(new URL('../src/operator/modelCatalog.ts', import.meta.url), 'utf8');

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
  assert.match(builder, /\{ id: 'created_at', type: 'timestamp', usage: true \}/);
  assert.match(builder, /\{ id: 'key_alias', type: 'text', usage: true \}/);
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

test('system filter assistant configuration persists a route reference rather than a secret', () => {
  assert.match(settings, /FilterAssistantSettings/);
  assert.match(settings, /model_route_id/);
  assert.doesNotMatch(settings, /credential_secret|api[_-]?key|client_secret/i);
});
