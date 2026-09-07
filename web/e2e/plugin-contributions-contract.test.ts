import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import type { PluginManifest } from '../src/types.js';
import { pluginRouteKey } from '../src/app/routes.js';
import { registerOperatorPluginContributions } from '../src/operator/pluginContributions.js';

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
  assert.match(source, /\/internal\/v1\/plugins\//);
  assert.doesNotMatch(source, /dangerouslySetInnerHTML|<iframe|import\s*\(/u);
  assert.doesNotMatch(source, /contribution\.url|endpoint\.url/u);
});
