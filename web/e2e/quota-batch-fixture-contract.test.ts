import assert from 'node:assert/strict';
import test from 'node:test';
import { quotaBatchFixtureResponse, quotaReadFixtureResponse } from './support/quota-batch.js';

test('quota batch fixture answers exactly the complete eligible set', () => {
  const accounts = [
    { id: 'account-a', tenant: 'fixture-a', driver: 'openai-codex' },
    { id: 'account-b', tenant: 'fixture-a', driver: 'http-json' },
  ];
  const response = quotaBatchFixtureResponse({ account_ids: ['account-a', 'account-b'], fresh: true, trigger: 'bulk' }, accounts);
  assert.equal(response.contract_version, 'upstream_quota_batch_v1');
  assert.deepEqual(response.results.map(result => result.upstream_account_id), ['account-a', 'account-b']);
  assert.ok(response.results.every(result => result.status === 'success'
    && result.snapshot.contract_version === 'upstream_quota_v1'
    && result.snapshot.tenant_external_id === 'fixture-a'));
  const read = quotaReadFixtureResponse(new URL('https://fixture.invalid/internal/v1/upstreams/account-b/quota?tenant_external_id=fixture-a&fresh=true&trigger=bulk'), accounts);
  assert.equal(read.upstream_account_id, 'account-b');
  assert.equal(read.tenant_external_id, 'fixture-a');
});

test('quota batch fixture rejects subset, unknown, duplicate, empty and non-bulk reads', () => {
  const accounts = [{ id: 'account-a', tenant: 'fixture-a' }, { id: 'account-b', tenant: 'fixture-a' }];
  assert.throws(() => quotaBatchFixtureResponse({ account_ids: ['account-a'], fresh: true, trigger: 'bulk' }, accounts), /exactly the eligible accounts/);
  assert.throws(() => quotaBatchFixtureResponse({ account_ids: ['account-a', 'account-x'], fresh: true, trigger: 'bulk' }, accounts), /exactly the eligible accounts/);
  assert.throws(() => quotaBatchFixtureResponse({ account_ids: ['account-a', 'account-a'], fresh: true, trigger: 'bulk' }, accounts), /exactly the eligible accounts/);
  assert.throws(() => quotaBatchFixtureResponse({ account_ids: [], fresh: true, trigger: 'bulk' }, accounts), /fresh bulk read/);
  assert.throws(() => quotaBatchFixtureResponse({ account_ids: ['account-a', 'account-b'], fresh: false, trigger: 'bulk' }, accounts), /fresh bulk read/);
  assert.throws(() => quotaBatchFixtureResponse({ account_ids: ['account-a', 'account-b'], fresh: true, trigger: 'manual' }, accounts), /fresh bulk read/);
  assert.throws(() => quotaReadFixtureResponse(new URL('https://fixture.invalid/internal/v1/upstreams/account-x/quota?tenant_external_id=fixture-a&fresh=true&trigger=bulk'), accounts), /eligible account and tenant/);
  assert.throws(() => quotaReadFixtureResponse(new URL('https://fixture.invalid/internal/v1/upstreams/account-a/quota?tenant_external_id=other&fresh=true&trigger=bulk'), accounts), /eligible account and tenant/);
  assert.throws(() => quotaReadFixtureResponse(new URL('https://fixture.invalid/internal/v1/upstreams/account-a/quota?tenant_external_id=fixture-a&fresh=false&trigger=bulk'), accounts), /fresh account read/);
});
