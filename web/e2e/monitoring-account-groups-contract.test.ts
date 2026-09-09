import assert from 'node:assert/strict';
import test from 'node:test';
import { monitoringAccountGroups } from '../src/operator/monitoringAccountGroups.js';
import type { OperatorMonitoringSnapshot } from '../src/types.js';

test('group repeated account names by stable identity without losing or recomputing model facts', () => {
  const make = (id: string, model: string, requests: number): OperatorMonitoringSnapshot['top_upstream_models'][number] => ({
    upstream_account_id: id, upstream_name: 'Shared account name', model,
    metrics: { requests, successful_requests: requests, failed_requests: 0, avg_duration_ms: 100, p95_duration_ms: 250, costs: [] },
    health: { version: 'upstream_breaker_v1', status: 'unknown', observed_at: null }, terminal_outcomes: [],
  });
  const rows = [make('account-a', 'model-one', 8), make('account-b', 'model-one', 4), make('account-a', 'model-two', 2)];
  const groups = monitoringAccountGroups(rows);
  assert.deepEqual(groups.map((group) => group.id), ['account-a', 'account-b']);
  assert.deepEqual(groups[0].models.map((row) => row.model), ['model-one', 'model-two']);
  assert.equal(groups[0].models[0], rows[0]);
  assert.equal(groups[0].models[1], rows[2]);
  assert.equal(groups[1].models[0], rows[1]);
  assert.deepEqual(monitoringAccountGroups([]), []);
});
