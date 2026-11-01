import assert from 'node:assert/strict';
import test from 'node:test';
import { strategyAccounts } from '../src/operator/groupStrategyResources.js';
import type { GroupView, ModelRouteView } from '../src/types.js';

test('strategy account choices respect group membership, server empty candidate ranges and provider exclusions', () => {
  const accounts = ['a', 'b', 'c', 'outside'].map(value => ({ value, label: value }));
  const group = { member_ids: ['r'] } as GroupView;
  const providers = [{ id: 'included', member_ids: ['a', 'b'] }, { id: 'excluded', member_ids: ['b'] }] as GroupView[];
  const route = { id: 'r', upstream_account_ids: ['c'], included_provider_group_ids: ['included'], excluded_provider_group_ids: ['excluded'] } as ModelRouteView;
  assert.deepEqual(strategyAccounts({ member_ids: ['a', 'missing'] } as GroupView, 'provider', accounts, [], []), [accounts[0]]);
  assert.deepEqual(strategyAccounts(group, 'route', accounts, [route], providers).map(value => value.value), ['a', 'c']);
  assert.deepEqual(strategyAccounts(group, 'route', accounts, [{ ...route, candidate_upstream_account_ids: [] }], providers), []);
  assert.deepEqual(strategyAccounts(group, 'route', accounts, [{ ...route, candidate_upstream_account_ids: ['b', 'missing'] }], providers).map(value => value.value), ['b']);
  assert.deepEqual(strategyAccounts({ member_ids: ['other'] } as GroupView, 'route', accounts, [route], providers), []);
});
