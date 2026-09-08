import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

import { filterResourceListByStatus, resourceListStatusStorageKey } from '../src/operator/ResourceListStatusFilter.js';

test('resource status filtering defaults to normal and only reveals inactive items on an explicit all selection', () => {
  const resources = [
    { id: 'active', status: 'active' },
    { id: 'suspended', status: 'suspended' },
    { id: 'revoked', status: 'revoked' },
  ];

  assert.deepEqual(
    filterResourceListByStatus(resources, 'normal', (resource) => resource.status === 'active').map((resource) => resource.id),
    ['active'],
  );
  assert.deepEqual(
    filterResourceListByStatus(resources, 'all', (resource) => resource.status === 'active').map((resource) => resource.id),
    ['active', 'suspended', 'revoked'],
  );
});

test('the saved status choice is scoped by resource kind and tenant, never by a credential value', () => {
  assert.equal(
    resourceListStatusStorageKey('upstreams', 'tenant-a'),
    'mtc.operator.resource-list-status.upstreams.tenant-a',
  );
  assert.notEqual(
    resourceListStatusStorageKey('upstreams', 'tenant-a'),
    resourceListStatusStorageKey('upstreams', 'tenant-b'),
  );
  assert.equal(
    resourceListStatusStorageKey('model-routes', ''),
    'mtc.operator.resource-list-status.model-routes.all-tenants',
  );
});

test('every status-bearing operator resource list uses the shared filter rather than a page-local disabled toggle', async () => {
  const source = await readFile(new URL('../src/operator/pages/ManagementPages.tsx', import.meta.url), 'utf8');
  const tenantSource = await readFile(new URL('../src/operator/TenantManager.tsx', import.meta.url), 'utf8');

  for (const resource of ['upstreams', 'model-routes', 'credentials', 'service-credentials']) {
    assert.match(source, new RegExp(`useResourceListStatusFilter\\('${resource}'`));
  }
  assert.match(source, /<ResourceListStatusFilterControl filter=\{statusFilter\}/);
  assert.match(source, /<ResourceListStatusEmpty totalCount=\{statusFilter\.totalCount\}/);
  assert.doesNotMatch(source, /showDisabled|setShowDisabled/);
  assert.match(tenantSource, /useResourceListStatusFilter\('tenants', '', values \?\? \[\], \(value\) => value\.status === 'active'\)/);
  assert.match(tenantSource, /<ResourceListStatusFilterControl filter=\{statusFilter\} inactiveLabel=\{t\('tenants\.archived'\)\}/);
  assert.match(tenantSource, /<ResourceListStatusEmpty totalCount=\{statusFilter\.totalCount\} normalLabel=\{t\('tenants\.active'\)\}/);
});
