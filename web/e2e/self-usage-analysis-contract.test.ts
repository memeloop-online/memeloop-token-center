import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const source = await readFile(new URL('../src/self/UsagePage.tsx', import.meta.url), 'utf8');
const sharedMetrics = await readFile(new URL('../src/operator/UsageSummaryMetrics.tsx', import.meta.url), 'utf8');

test('self usage uses the credential-scoped analysis endpoint and never sends operator scope selectors', () => {
  assert.match(source, /\/self\/v1\/usage-analysis/);
  assert.match(source, /from_created_at/);
  assert.match(source, /to_created_at/);
  assert.doesNotMatch(source, /tenant_external_id|key_id|principal|route_id|upstream_account_id|session_id/);
});

test('self usage exposes complete KPI and accessible chart families', () => {
  for (const marker of ['throughputOption', 'latencyOption', 'costOption', 'heatmapOption', 'ChartDataTable']) {
    assert.match(source, new RegExp(marker));
  }
  for (const marker of ['cached_input_tokens', 'cache_write_tokens', 'avg_duration_ms', 'p95_duration_ms', 'AnalyticsMetric', 'averageTps', 'localSettlementLabel']) {
    assert.match(sharedMetrics, new RegExp(marker));
  }
  assert.match(source, /<Suspense/);
  assert.match(source, /CostLines/);
});

test('self usage reuses the shared analytics summary with credential currency isolation', () => {
  assert.match(source, /<UsageSummaryMetrics stats=\{stats\} currency=\{credentialView\.currency\} timeZone=\{timeZone\} \/>/);
  assert.doesNotMatch(source, /<NumberMetric|<Metric\b/);
  assert.match(sharedMetrics, /export type UsageSummaryStats/);
  assert.match(sharedMetrics, /selectedCosts = currency \? summary\.costs\.filter/);
});
