import assert from 'node:assert/strict';
import test from 'node:test';
import { monitoringAccountGroups, monitoringModelGroups } from '../src/operator/monitoringAccountGroups.js';
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

test('model headings are unique while Copilot, Cursor and Kimi keep their own account facts', () => {
  const rows = ['Copilot', 'Cursor', 'Kimi'].map((name, index) => ({
    upstream_account_id: `account-${index}`, upstream_name: name, model: 'shared-model',
    metrics: { requests: index + 1, successful_requests: index, failed_requests: 1,
      avg_duration_ms: 100 + index, p95_duration_ms: 250 + index,
      costs: [{ currency: index === 1 ? 'EUR' : 'USD', cost: '0.123456789' }] },
    health: { version: 'upstream_breaker_v1' as const, status: 'unknown' as const, observed_at: null }, terminal_outcomes: [],
  }));
  const secondModel = { ...rows[0], model: 'other-model' };
  const groups = monitoringModelGroups([...rows, rows[0], secondModel]);
  assert.deepEqual(groups.map((group) => group.model), ['shared-model', 'other-model']);
  assert.deepEqual(groups[0].accounts.map((row) => row.upstream_name), ['Copilot', 'Cursor', 'Kimi']);
  rows.forEach((row, index) => assert.equal(groups[0].accounts[index], row, 'metrics remain exact server facts, not summed costs or averaged percentiles'));
  assert.equal(groups[1].accounts[0], secondModel);
  assert.deepEqual(monitoringModelGroups([]), []);
});
