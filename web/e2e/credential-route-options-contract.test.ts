import assert from 'node:assert/strict';
import test from 'node:test';
import type { ModelRouteView, ProviderType, UpstreamAccount } from '../src/types.js';
import { credentialRouteOptions } from '../src/operator/credentialRouteOptions.js';

const route = (id: string, overrides: Partial<ModelRouteView> = {}): ModelRouteView => ({ id, public_model: 'Sol', upstream_model: 'sol', protocol: 'openai', enabled: true, priority: 1, created_at: 1, updated_at: 1, grant_revision: 1, ...overrides });
test('route grant identity, candidate count and unavailable reasons are independent from model labels', () => {
  const accounts = [{ id: 'a', name: 'Same name', driver: 'kimi' }, { id: 'b', name: 'Same name', driver: 'kimi' }] as UpstreamAccount[];
  const providers = [{ id: 'kimi', display_name: 'Kimi' }] as ProviderType[];
  const input = [route('one', { candidate_upstream_account_ids: ['a'] }), route('two', { candidate_upstream_account_ids: ['b'] }), route('many', { candidate_upstream_account_ids: ['a', 'b'] }), route('off', { enabled: false, candidate_upstream_account_ids: [] }), route('empty', { candidate_upstream_account_ids: [] }), route('unknown')];
  const options = credentialRouteOptions([...input, input[0]], accounts, providers, 'zh-CN');
  assert.deepEqual(options.map(option => option.value), input.map(value => value.id));
  assert.notEqual(options[0].label, options[1].label);
  assert.match(options[2].description!, /此路由含 2 个账号/);
  assert.match(options[3].description!, /已停用/);
  assert.match(options[4].description!, /无可用候选账号/);
  assert.match(options[5].description!, /候选目录未知/);
  assert.equal(options[3].disabled, true);
  assert.equal(options[4].disabled, true);
  assert.equal(options[5].disabled, false, 'missing catalog metadata must not be misreported as an empty candidate set');
});

test('technical IDs are tooltip details and duplicate labels use collision-safe short suffixes', () => {
  const first = '00000000-0000-4000-8000-00000000abcd';
  const second = '00000000-0000-4000-8000-00000001abcd';
  const options = credentialRouteOptions([route(first), route(second)], [], [], 'zh-CN');
  assert.notEqual(options[0].label, options[1].label);
  for (const option of options) {
    assert.equal(option.label.includes(option.value), false);
    assert.equal(option.description.includes(option.value), false);
    assert.ok(option.details.includes(option.value));
  }
});
