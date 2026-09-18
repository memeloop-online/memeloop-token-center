import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import type { PluginManifest } from '../src/types.js';
import { pluginRouteKey } from '../src/app/routes.js';
import { healthIntelligenceSnapshot, registerOperatorPluginContributions } from '../src/operator/pluginContributions.js';

const fixture = JSON.parse(await readFile(new URL('../../tests/fixtures/plugins/operator-ui-contributions.json', import.meta.url), 'utf8')) as {
  installed: PluginManifest[];
  conflicting_category: PluginManifest;
  uninstalled: PluginManifest[];
};

test('plugin fixtures add Health and intelligence to Monitoring and can add a named category', () => {
  const registry = registerOperatorPluginContributions(fixture.installed);
  const monitoring = registry.navigation.find((section) => section.id === 'monitoring');
  assert.deepEqual(monitoring?.items.map((item) => item.label), ['Health and intelligence']);
  const intelligence = registry.navigation.find((section) => section.id === 'intelligence');
  assert.equal(intelligence?.label, 'Intelligence');
  assert.deepEqual(intelligence?.items.map((item) => item.label), ['Signals']);
  assert.ok(registry.pages.has(pluginRouteKey('observability-suite', 'health-intelligence')));
  assert.equal(registry.overviewCards.length, 1);
});

test('conflicting category labels fail closed instead of replacing registered navigation', () => {
  const registry = registerOperatorPluginContributions([...fixture.installed, fixture.conflicting_category]);
  const intelligence = registry.navigation.find((section) => section.id === 'intelligence');
  assert.equal(intelligence?.label, 'Intelligence');
  assert.ok(registry.pages.has(pluginRouteKey('observability-suite', 'intelligence-signals')));
  assert.equal(registry.pages.has(pluginRouteKey('different-intelligence', 'conflicting-signals')), false);
});

test('uninstalling a manifest removes every plugin navigation, route, and card registration', () => {
  const before = registerOperatorPluginContributions(fixture.installed);
  assert.ok(before.navigation.length > 0 && before.pages.size > 0 && before.overviewCards.length > 0);
  const after = registerOperatorPluginContributions(fixture.uninstalled);
  assert.deepEqual(after.navigation, []);
  assert.equal(after.pages.size, 0);
  assert.deepEqual(after.overviewCards, []);
});

test('projection presentation uses existing page/card registry with manifest revision and link grants', () => {
  const installed = structuredClone(fixture.installed);
  installed[0].contributions.operator_ui!.forEach((contribution) => { contribution.presentation = 'projection_v1'; });
  const registry = registerOperatorPluginContributions(installed);
  assert.equal(registry.overviewCards.length, 1);
  assert.ok(registry.pages.size > 0);
  const registered = registry.overviewCards[0]!;
  assert.equal(registered.manifestRevision, JSON.stringify(installed[0]));
  assert.deepEqual(registered.allowedLinkOrigins, installed[0].capabilities.flatMap((capability) => capability.kind === 'http' ? capability.allowed_origins : []));
});

test('component contributions register from immutable runtime module identities', () => {
  const installed = structuredClone(fixture.installed);
  installed[0].contributions.operator_ui!.push(
    {
      id: 'component-tab', slot: 'operator.sidebar.tab', category: { id: 'monitoring' }, route: 'component-tab',
      label: 'Component tab', icon: 'plug', renderer: 'component_v1', module_entry: 'assets/operator-ui.mjs', module_sha256: `sha256:${'a'.repeat(64)}`, component_id: 'workspace', component_props: { density: 'compact' },
    },
    {
      id: 'provider-extension', slot: 'operator.page.after', target_route: 'providers',
      label: 'Provider extension', icon: 'plug', renderer: 'component_v1', module_entry: 'assets/operator-ui.mjs', module_sha256: `sha256:${'a'.repeat(64)}`, component_id: 'workspace',
    },
  );
  const registry = registerOperatorPluginContributions(installed);
  assert.equal(registry.pages.get(pluginRouteKey('observability-suite', 'component-tab'))?.contribution.module_entry, 'assets/operator-ui.mjs');
  assert.deepEqual(registry.pageExtensions.get('providers')?.after.map((value) => value.contribution.id), ['provider-extension']);
});

test('browser registry rejects malformed renderer contracts and module paths', () => {
  const installed = structuredClone(fixture.installed);
  installed[0].contributions.operator_ui = [
    {
      id: 'typed-with-component-state', slot: 'operator.overview.card', label: 'Invalid typed data', icon: 'plug',
      renderer: 'typed_data_v1', data_endpoint: 'health', component_props: { unexpected: true },
    },
    {
      id: 'component-with-empty-endpoint', slot: 'operator.overview.card', label: 'Invalid endpoint', icon: 'plug',
      renderer: 'component_v1', module_entry: 'assets/operator-ui.mjs', module_sha256: `sha256:${'a'.repeat(64)}`, component_id: 'workspace', data_endpoint: '',
    },
    {
      id: 'component-with-normalized-path', slot: 'operator.overview.card', label: 'Invalid path', icon: 'plug',
      renderer: 'component_v1', module_entry: 'assets//operator-ui.mjs', module_sha256: `sha256:${'a'.repeat(64)}`, component_id: 'workspace',
    },
  ];
  const registry = registerOperatorPluginContributions(installed);
  assert.deepEqual(registry.overviewCards, []);
});

test('render boundary loads only digest-addressed same-origin modules and exposes no generic request API', async () => {
  const source = await readFile(new URL('../src/operator/pluginContributions.tsx', import.meta.url), 'utf8');
  const host = await readFile(new URL('../src/plugins/OperatorPluginComponentHost.tsx', import.meta.url), 'utf8');
  assert.match(source, /renderer === 'typed_data_v1'/);
  assert.match(source, /renderer === 'component_v1'/);
  assert.match(source, /health_intelligence_v1/);
  assert.match(source, /\/internal\/v1\/plugins\//);
  assert.doesNotMatch(source, /dangerouslySetInnerHTML|<iframe/u);
  assert.doesNotMatch(source, /contribution\.url|endpoint\.url/u);
  assert.match(host, /new URL\(expectedPath, window\.location\.origin\)/);
  assert.match(host, /import\(\/\* @vite-ignore \*\/ url\)/);
  assert.match(host, /PluginContributionBoundary/);
  assert.doesNotMatch(host, /async request|OperatorUiRequestOptions/u);
});

test('equal raw routes remain independent across plugin namespaces', () => {
  const installed = structuredClone(fixture.installed);
  const second = structuredClone(installed[0]);
  second.id = 'another-observability-suite';
  second.contributions.operator_ui = second.contributions.operator_ui?.filter((value) => value.slot === 'operator.sidebar.tab').map((value) => ({ ...value, route: 'health-intelligence' }));
  const registry = registerOperatorPluginContributions([...installed, second]);
  assert.ok(registry.pages.has(pluginRouteKey('observability-suite', 'health-intelligence')));
  assert.ok(registry.pages.has(pluginRouteKey('another-observability-suite', 'health-intelligence')));
});

test('the closed health-intelligence presentation only accepts its bounded three-source snapshot', () => {
  const snapshot = healthIntelligenceSnapshot({
    schemaVersion: 1,
    generatedAt: '2026-09-08T00:00:00.000Z',
    sources: [
      { id: 'codexradar', label: 'Codex Radar', status: 'ok', rows: [{ model: 'model-a', effort: 'high', iq: 42.5, samples: 12 }] },
      { id: 'deepswe', label: 'DeepSWE', status: 'stale', rows: [{ model: 'model-b', effort: 'medium', passRate: 0.75, agentSteps: 8 }] },
      { id: 'aixhan', label: 'Model health', status: 'error', rows: [{ name: 'provider-a', status: 'degraded', model: 'model-c', latencyMs: 320 }] },
    ],
  });
  assert.deepEqual(snapshot?.sources.map((source) => source.id), ['codexradar', 'deepswe', 'aixhan']);
  assert.equal(snapshot?.sources[0]?.rows[0]?.title, 'model-a · high');
  assert.equal(snapshot?.sources[2]?.rows[0]?.value, 'degraded');
  assert.equal(healthIntelligenceSnapshot({ schemaVersion: 1, sources: [] }), null);
  assert.equal(healthIntelligenceSnapshot({ schemaVersion: 1, sources: [
    { id: 'codexradar', label: 'Codex Radar', status: 'ok', rows: [] },
    { id: 'codexradar', label: 'Duplicate', status: 'ok', rows: [] },
    { id: 'aixhan', label: 'Model health', status: 'ok', rows: [] },
  ] }), null);
});
