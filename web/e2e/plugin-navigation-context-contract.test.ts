import assert from 'node:assert/strict';
import test from 'node:test';
import type { PluginManifest, PluginOperatorUiContribution } from '../src/types.js';
import { registerOperatorPluginContributions } from '../src/operator/pluginContributions.js';
import { resolvePluginNavigationContext } from '../src/operator/pluginNavigationContext.js';

test('navigation context never crosses a route, tenant or authenticated credential generation', () => {
  const context = { route: 'plugin:extension:workspace', tenant: 'one', credentialGeneration: 3, value: { modelRoute: { id: 'route-a' } } };
  assert.equal(resolvePluginNavigationContext(context, context.route, 3, 'one'), context.value);
  assert.equal(resolvePluginNavigationContext(context, 'routes', 3, 'one'), undefined);
  assert.equal(resolvePluginNavigationContext(context, context.route, 4, 'one'), undefined);
  assert.equal(resolvePluginNavigationContext(context, context.route, 3, 'two'), undefined);
  assert.equal(resolvePluginNavigationContext(undefined, context.route, 3, 'one'), undefined);
});

test('row actions register only components without page navigation declarations', () => {
  const action: PluginOperatorUiContribution = {
    id: 'open-route', slot: 'operator.model-route.action', label: 'Open route', icon: 'plug',
    renderer: 'component_v1', module_entry: 'ui/actions.mjs', module_sha256: `sha256:${'a'.repeat(64)}`, component_id: 'open-route',
  };
  const manifest: PluginManifest = { id: 'extension', version: '1.0.0', wit_version: '0.2.0', capabilities: [], contributions: { operator_ui: [action] } };
  const registry = registerOperatorPluginContributions([manifest]);
  assert.deepEqual(registry.modelRouteActions.map(value => value.contribution.id), ['open-route']);
  assert.equal(registry.pages.size, 0);
  assert.deepEqual(registry.navigation, []);
  for (const extra of [{ route: 'page' }, { category: { id: 'monitoring' } }, { target_route: 'routes' }, { renderer: 'typed_data_v1', module_entry: undefined, module_sha256: undefined, component_id: undefined, data_endpoint: 'data' }]) {
    const invalid = structuredClone(manifest);
    invalid.contributions.service_data = [{ id: 'data', required_scope: 'metrics:read', response_schema: {}, fallback: {}, cache_ttl_seconds: 60, timeout_millis: 1000, max_body_bytes: 1024 }];
    invalid.contributions.operator_ui = [{ ...action, ...extra } as PluginOperatorUiContribution];
    assert.deepEqual(registerOperatorPluginContributions([invalid]).modelRouteActions, []);
  }
});
