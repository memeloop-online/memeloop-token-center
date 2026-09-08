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

test('render boundary remains core-owned typed JSON with no remote executable surface', async () => {
  const source = await readFile(new URL('../src/operator/pluginContributions.tsx', import.meta.url), 'utf8');
  assert.match(source, /renderer === 'typed_data_v1'/);
  assert.match(source, /health_intelligence_v1/);
  assert.match(source, /\/internal\/v1\/plugins\//);
  assert.doesNotMatch(source, /dangerouslySetInnerHTML|<iframe|import\s*\(/u);
  assert.doesNotMatch(source, /contribution\.url|endpoint\.url/u);
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
