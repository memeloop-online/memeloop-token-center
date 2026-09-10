import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import { routeModelOptions } from '../src/operator/modelCatalog.js';
import type { GroupView, ModelRouteView, UpstreamAccount } from '../src/types.js';

test('model hierarchy uses actual account membership, excludes removed accounts, and preserves route/public IDs', () => {
  const route = { id: 'route-1', public_model: 'not-a-provider/model', upstream_model: 'native-model', protocol: 'openai', included_provider_group_ids: ['group-1'], excluded_provider_group_ids: ['group-2'] } as ModelRouteView;
  const accounts = [{ id: 'a', name: 'Account A', driver: 'actual-provider' }, { id: 'b', name: 'Account B', driver: 'other-provider' }] as UpstreamAccount[];
  const groups = [{ id: 'group-1', member_ids: ['a', 'b'] }, { id: 'group-2', member_ids: ['b'] }] as GroupView[];
  const options = routeModelOptions([route], accounts, groups, 'unknown');
  assert.deepEqual(options, [{ key: 'route-1:a', value: 'not-a-provider/model', label: 'not-a-provider/model', provider: 'actual-provider', upstream: 'Account A', description: 'native-model · openai' }]);
  assert.equal(routeModelOptions([route], accounts, groups, 'unknown', 'route')[0].value, 'route-1');
  assert.equal(routeModelOptions([route], [], [], 'unknown')[0].provider, 'unknown');
});

test('all maintained model selectors share the same picker without replacing catalog coverage validation', async () => {
  for (const path of ['operator/TypedFilterBuilder.tsx', 'operator/UpstreamModelCombobox.tsx', 'operator/pages/SystemSettingsPage.tsx', 'self/GeneratePage.tsx']) {
    assert.match(await readFile(new URL(`../src/${path}`, import.meta.url), 'utf8'), /<ModelPicker/);
  }
  const upstream = await readFile(new URL('../src/operator/UpstreamModelCombobox.tsx', import.meta.url), 'utf8');
  assert.match(upstream, /selected\.complete_coverage \|\| \(confirmationScope === scopeKey && partialConfirmed\)/);
  assert.match(upstream, /needsCustomConfirmation && customAllowed && confirmationScope === scopeKey && customConfirmed/);
  assert.match(upstream, /catalogResult\?\.scopeKey === scopeKey \? catalogResult\.data : undefined/);
  assert.match(upstream, /validityCallback\.current\(\{ scopeKey, valid, allowCustom \}\)/);
  const management = await readFile(new URL('../src/operator/pages/ManagementPages.tsx', import.meta.url), 'utf8');
  assert.match(management, /catalog\.scopeKey === currentScopeKey && catalog\.valid/);
  assert.match(management, /!canSubmit\(draft, catalog\)\) return/);
  assert.match(upstream, /Math\.min\(4, ids\.length\)/);
  const picker = await readFile(new URL('../src/ModelPicker.tsx', import.meta.url), 'utf8');
  assert.match(picker, /popover="auto"/);
  assert.match(picker, /aria-activedescendant/);
  assert.match(picker, /event\.key === 'Escape'/);
  assert.doesNotMatch(picker, /setTimeout|aria-modal="true"/);
});
