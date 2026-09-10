import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { tenantForCredential } from '../src/credentialStorage.js';
import { operatorRouteKeys } from '../src/operator/scope/operatorRoutes.js';
import { routeCandidatePreview } from '../src/operator/routeCandidatePreview.js';
import type { GroupView, ModelRouteView, UpstreamAccount } from '../src/types.js';

const operator = readFileSync(new URL('../src/operator/Operator.tsx', import.meta.url), 'utf8');
const scope = readFileSync(new URL('../src/operator/hooks/useOperatorScope.ts', import.meta.url), 'utf8');
const requestsPage = readFileSync(new URL('../src/operator/pages/RequestsPage.tsx', import.meta.url), 'utf8');
const managementPages = readFileSync(new URL('../src/operator/pages/ManagementPages.tsx', import.meta.url), 'utf8');
const resourceHook = readFileSync(new URL('../src/operator/hooks/useOperatorResource.ts', import.meta.url), 'utf8');
const resourceListStatusFilter = readFileSync(new URL('../src/operator/ResourceListStatusFilter.tsx', import.meta.url), 'utf8');
const typedFilterBuilder = readFileSync(new URL('../src/operator/TypedFilterBuilder.tsx', import.meta.url), 'utf8');

test('operator exposes controlled built-in and plugin routing without coupling credentials to the URL', () => {
  assert.match(operator, /type OperatorApplicationRoute = OperatorRouteKey \| PluginRouteKey/);
  assert.match(operator, /route\?: OperatorApplicationRoute/);
  assert.match(operator, /onRouteChange\?: \(route: OperatorApplicationRoute\)/);
  assert.match(operator, /embedded\?: boolean/);
  assert.match(operator, /showNavigation\?: boolean/);
  assert.doesNotMatch(operator, /URLSearchParams|location\.|history\./);
});

test('sessions are a first-class operator route and all page keys are explicit', () => {
  assert.deepEqual(operatorRouteKeys, [
    'overview', 'requests', 'sessions', 'usage', 'generations', 'providers', 'routes',
    'pricing', 'tenants', 'credentials', 'service-credentials', 'plugins', 'settings',
  ]);
  assert.match(operator, /case 'sessions': page = <SessionsPage/);
  assert.match(operator, /case 'tenants': page = <TenantManager/);
  assert.doesNotMatch(operator, /trafficMode|onModeChange/);
});

test('credential authentication discovers only tenants before a page mounts', () => {
  assert.match(scope, /api<TenantView\[]>\('\/internal\/v1\/tenants'/);
  assert.doesNotMatch(scope, /provider-types|plugins|upstreams|requests|schemas/);
});

test('operator restores an allowed tenant and otherwise selects an explicit tenant', () => {
  const tenants = [{ external_id: 'archive' }, { external_id: 'default' }];
  assert.equal(tenantForCredential(tenants, 'archive'), 'archive');
  assert.equal(tenantForCredential(tenants, 'missing'), 'default');
  assert.equal(tenantForCredential([{ external_id: 'only' }], ''), 'only');
  assert.equal(tenantForCredential([{ external_id: 'one' }, { external_id: 'two' }], ''), 'one');
});

test('credential review defaults to active and mutation scopes remain explicit', () => {
  assert.match(managementPages, /useResourceListStatusFilter\('credentials', tenant, nonStatusFilteredValues, \(value\) => \(value\.status \?\? 'active'\) === 'active'\)/);
  assert.match(resourceListStatusFilter, /return window\.localStorage\.getItem\(key\) === 'all' \? 'all' : 'normal';/);
  assert.match(resourceListStatusFilter, /useState<ResourceListStatusSelection>\(\(\) => readSelection\(storageKey\)\)/);
  assert.match(scope, /const writeTenant = state\.tenant;/);
  assert.doesNotMatch(operator, /<option value="">/);
});

test('request filters hide stale rows and cursors while a replacement query is pending or fails', () => {
  assert.match(requestsPage, /if \(!older\) \{\s+olderFilteredResultsVisible\.current = false;\s+setOlderFilteredResultsStale\(false\);\s+setRequests\(\[\]\); setHasOlder\(false\); setDetail\(undefined\);\s+\}/);
  assert.match(requestsPage, /setUpstreamError\(messageOf/);
  assert.match(requestsPage, /<TypedFilterBuilder ast=\{filters\} disabled=\{loading\} onApply=\{onApply\} onClear=\{onClear\} scope="requests" token=\{token\} tenant=\{tenant\} upstreams=\{upstreams\} \/>/);
  assert.match(typedFilterBuilder, /api<ModelRouteView\[\]>\(`\/internal\/v1\/model-routes\$\{query\}`/);
  assert.match(typedFilterBuilder, /routeModelOptions\(routes, upstreams, groups/);
  assert.match(typedFilterBuilder, /<ModelPicker/);
  assert.doesNotMatch(typedFilterBuilder, /\/internal\/v1\/upstream-models/);
  assert.match(typedFilterBuilder, /upstreams\.filter\(\(account\) => account\.status === 'active'\)\.map\(\(account\) => <option value=\{account\.id\}/);
});

test('pricing sync and resource refreshes retain only current operation results', () => {
  assert.match(managementPages, /const syncSequence = useRef\(0\)/);
  assert.match(managementPages, /sequence !== syncSequence\.current/);
  assert.match(managementPages, /syncSequence\.current \+= 1; setSyncing\(false\)/);
  assert.match(resourceHook, /refreshError\?: string/);
  assert.match(resourceHook, /refreshError: action\.message/);
});

test('route candidate previews preserve explicit, included, excluded and missing provenance', () => {
  const route = {
    upstream_account_ids: ['explicit', 'also-grouped'],
    included_provider_group_ids: ['included', 'missing'],
    excluded_provider_group_ids: ['excluded'],
  } as ModelRouteView;
  const groups = [
    { id: 'included', name: 'Codex OAuth', member_ids: ['also-grouped', 'group-only'] },
    { id: 'excluded', name: 'Draining', member_ids: ['group-only'] },
  ] as GroupView[];
  const accounts = [
    { id: 'explicit', name: 'Explicit', status: 'active' },
    { id: 'also-grouped', name: 'Both', status: 'active' },
    { id: 'group-only', name: 'Excluded', status: 'active' },
  ] as UpstreamAccount[];

  const preview = routeCandidatePreview(route, groups, accounts);
  assert.deepEqual(preview.missingGroupIds, ['missing']);
  assert.equal(preview.configuredCount, 2);
  assert.equal(preview.excludedCount, 1);
  assert.equal(preview.candidates.find((candidate) => candidate.id === 'also-grouped')?.explicit, true);
  assert.deepEqual(
    preview.candidates.find((candidate) => candidate.id === 'also-grouped')?.includedBy.map((group) => group.name),
    ['Codex OAuth'],
  );
  assert.deepEqual(
    preview.candidates.find((candidate) => candidate.id === 'group-only')?.excludedBy.map((group) => group.name),
    ['Draining'],
  );
});

test('routing UI distinguishes candidate pools, permission bundles and saved effective access', () => {
  const candidateSummary = readFileSync(new URL('../src/operator/RouteCandidateSummary.tsx', import.meta.url), 'utf8');
  const groupManager = readFileSync(new URL('../src/operator/GroupManager.tsx', import.meta.url), 'utf8');
  const modelPicker = readFileSync(new URL('../src/ModelPicker.tsx', import.meta.url), 'utf8');
  const upstreamModel = readFileSync(new URL('../src/operator/UpstreamModelCombobox.tsx', import.meta.url), 'utf8');

  assert.match(managementPages, /<RouteCandidateSummary/);
  assert.match(candidateSummary, /configNotHealth/);
  assert.match(candidateSummary, /excludedBy/);
  assert.match(groupManager, /groupGrantHint/);
  assert.match(groupManager, /emptyRouteGroup/);
  assert.match(groupManager, /disabled=\{busy \|\| !membersChanged\}/);
  assert.match(modelPicker, /popover="auto"/);
  assert.match(modelPicker, /searchableEditable/);
  assert.match(upstreamModel, /browseModelQuery/);
  assert.match(upstreamModel, /onQueryChange=\{setSearchQuery\}/);
});
