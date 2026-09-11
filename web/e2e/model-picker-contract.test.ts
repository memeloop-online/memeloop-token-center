import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import { routeModelOptions } from '../src/operator/modelCatalog.js';
import type { GroupView, ModelRouteView, UpstreamAccount } from '../src/types.js';

test('model hierarchy uses actual account membership, excludes removed accounts, and preserves route/public IDs', () => {
  const route = { id: 'route-1', public_model: 'not-a-provider/model', upstream_model: 'native-model', protocol: 'openai', included_provider_group_ids: ['group-1'], excluded_provider_group_ids: ['group-2'] } as ModelRouteView;
  const accounts = [{ id: 'a', name: 'Account A', driver: 'actual-provider', status: 'active', credential_expires_at: null }, { id: 'b', name: 'Account B', driver: 'other-provider', status: 'disabled', credential_expires_at: null }] as UpstreamAccount[];
  const groups = [{ id: 'group-1', name: 'Preferred', member_ids: ['a', 'b'] }, { id: 'group-2', name: 'Excluded', member_ids: ['b'] }] as GroupView[];
  const options = routeModelOptions([route], accounts, groups, 'unknown');
  assert.deepEqual(options, [{
    key: 'route-1:a', value: 'not-a-provider/model', label: 'not-a-provider/model', providerGroup: 'Preferred', provider: 'actual-provider', upstream: 'Account A', description: 'native-model',
    availability: 'available', health: 'unknown', capabilities: ['openai'], disabled: false,
  }]);
  assert.equal(routeModelOptions([route], accounts, groups, 'unknown', 'route')[0].value, 'route-1');
  const unknown = routeModelOptions([route], [], [], 'unknown')[0];
  assert.equal(unknown.provider, 'unknown');
  assert.equal(unknown.availability, 'unknown');
  assert.equal(unknown.disabled, true);
});

test('all maintained model selectors share the same picker and stale catalogs remain safely restricted', async () => {
  for (const path of ['operator/TypedFilterBuilder.tsx', 'operator/UpstreamModelCombobox.tsx', 'operator/pages/SystemSettingsPage.tsx', 'self/GeneratePage.tsx']) {
    assert.match(await readFile(new URL(`../src/${path}`, import.meta.url), 'utf8'), /<ModelPicker/);
  }
  const upstream = await readFile(new URL('../src/operator/UpstreamModelCombobox.tsx', import.meta.url), 'utf8');
  assert.match(upstream, /const selectedValid = Boolean\(selected\)/);
  assert.match(upstream, /needsCustomConfirmation && customAllowed && customConfirmed/);
  assert.match(upstream, /needsCustomConfirmation = Boolean\(value\.trim\(\) && !selected\)/);
  assert.match(upstream, /routes\.catalogLastVerified/);
  assert.doesNotMatch(upstream, /confirmPartialCoverage|catalogNotReady|partialConfirmed/);
  assert.match(upstream, /Math\.min\(4, ids\.length\)/);
  const picker = await readFile(new URL('../src/ModelPicker.tsx', import.meta.url), 'utf8');
  assert.match(picker, /popover="auto"/);
  assert.match(picker, /aria-activedescendant/);
  assert.match(picker, /event\.key === 'Escape'/);
  assert.doesNotMatch(picker, /setTimeout|aria-modal="true"/);
});

test('filter-assistant choice exposes configuration availability without probing an upstream', async () => {
  const settings = await readFile(new URL('../src/operator/pages/SystemSettingsPage.tsx', import.meta.url), 'utf8');
  const catalog = await readFile(new URL('../src/operator/modelCatalog.ts', import.meta.url), 'utf8');
  assert.match(settings, /selectedRouteHasAvailableCandidate/);
  assert.match(settings, /filterAssistantRouteUnavailable/);
  assert.match(settings, /describedBy="filter-assistant-route-hint"/);
  assert.match(catalog, /credentialExpiresAt|credential_expires_at/);
  assert.match(catalog, /health: 'unknown'/);
  assert.match(catalog, /disabled: !available/);
  assert.doesNotMatch(settings, /\/health|\/models\/sync|filter-assistant\/plan/);
});
