import assert from 'node:assert/strict';
import test from 'node:test';

import { credentialAliasReady, credentialEditOperations } from '../src/operator/credentialEditPlan.js';
import type { KeyView } from '../src/types.js';

const policy: KeyView['policy'] = {
  requests_per_minute: 60,
  tokens_per_minute: 100_000,
  max_concurrency: 2,
  enforcement_mode: 'prepaid',
  daily_budget: null,
  weekly_budget: null,
  lifetime_budget: null,
};

const routing = {
  key_id: 'key-a',
  route_ids: ['route-b', 'route-a'],
  route_group_ids: ['group-a'],
  effective_route_ids: ['route-a', 'route-b'],
  updated_at: 1,
  grant_revision: 3,
};

test('an untouched edit draft produces no writes', () => {
  const operations = credentialEditOperations(
    { alias: 'Research workspace', policy, routing },
    { alias: 'Research workspace', policy: { ...policy }, routing: { route_ids: ['route-a', 'route-b'], route_group_ids: ['group-a'], grant_revision: 3 } },
    'tenant-a',
  );
  assert.deepEqual(operations, [], 'reordered selections and key order never rewrite unchanged sections');
});

test('each changed section maps to its existing endpoint, exactly once', () => {
  const operations = credentialEditOperations(
    { alias: 'Research workspace', policy, routing },
    {
      alias: 'Renamed workspace',
      policy: { ...policy, max_concurrency: 5 },
      routing: { route_ids: [], route_group_ids: ['group-a'], grant_revision: 3 },
    },
    'tenant-a',
  );
  assert.deepEqual(operations.map(operation => [operation.section, operation.method, operation.suffix]), [
    ['alias', 'PATCH', 'alias'],
    ['policy', 'PUT', 'policy'],
    ['routing', 'PUT', 'routing'],
  ]);
  assert.deepEqual(operations[0].body, { alias: 'Renamed workspace' }, 'the alias is submitted exactly as entered');
  assert.equal((operations[1].body as KeyView['policy']).max_concurrency, 5);
  assert.deepEqual(operations[2].body, {
    tenant_external_id: 'tenant-a',
    route_ids: [],
    route_group_ids: ['group-a'],
    expected_grant_revision: 3,
  }, 'routing saves carry the tenant and the concurrency guard revision');
});

test('routing stays unsaved until its current state has loaded', () => {
  const operations = credentialEditOperations(
    { alias: 'Research workspace', policy },
    { alias: 'Research workspace', policy },
    'tenant-a',
  );
  assert.deepEqual(operations, [], 'without a loaded routing baseline no routing write is possible');
});

test('the alias must stay non-blank before any save is offered', () => {
  assert.equal(credentialAliasReady('研发工作区'), true);
  assert.equal(credentialAliasReady(''), false);
  assert.equal(credentialAliasReady('   '), false, 'a whitespace-only alias is not a usable identity label');
});
