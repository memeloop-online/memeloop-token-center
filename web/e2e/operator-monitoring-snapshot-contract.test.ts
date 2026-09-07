import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

import type { OperatorMonitoringSnapshot } from '../src/types.js';

const pageSource = await readFile(new URL('../src/operator/pages/OperatorPages.tsx', import.meta.url), 'utf8');
const componentSource = await readFile(new URL('../src/operator/MonitoringSnapshot.tsx', import.meta.url), 'utf8');

const snapshot: OperatorMonitoringSnapshot = {
  contract_version: 'v1', generated_at: 2_000, scope: 'tenant', tenant_external_id: 'tenant-a',
  from_created_at: 1_000, to_created_at: 2_000, granularity: 'hour',
  latency_is_approximate: true, latency_method: 'fixed_histogram_upper_bound_capped_60000ms',
  summary: { requests: 0, successful_requests: 0, failed_requests: 0, avg_duration_ms: null, p95_duration_ms: null, costs: [] },
  freshness: { latest_terminal_created_at: null, age_millis: null },
  health: { version: 'upstream_breaker_v1', status: 'unknown', observed_at: null },
  top_upstream_models: [],
};

test('monitoring types preserve explicit window, currency arrays, and zero-traffic unknown health', () => {
  assert.equal(snapshot.scope, 'tenant');
  assert.equal(snapshot.summary.requests, 0);
  assert.equal(snapshot.health.status, 'unknown');
  assert.deepEqual(snapshot.summary.costs, []);
});

test('overview sends an explicit tenant/global scope and exact time bounds', () => {
  assert.match(pageSource, /scope: tenant \? 'tenant' : 'global'/);
  assert.match(pageSource, /from_created_at: String\(now - 86_400_000\)/);
  assert.match(pageSource, /to_created_at: String\(now\)/);
  assert.match(pageSource, /\/internal\/v1\/monitoring-snapshot/);
});

test('monitoring UI cannot show more than five terminal outcomes for a top pair', () => {
  assert.match(componentSource, /terminal_outcomes\.slice\(0, 5\)/);
  assert.match(componentSource, /monitoring\.health\.\$\{health\.status\}/);
  assert.match(componentSource, /monitoring\.noStableUpstreamTraffic/);
});
