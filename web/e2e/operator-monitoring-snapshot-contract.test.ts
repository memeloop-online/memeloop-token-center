import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

import { formatElapsedTime } from '../src/format.js';
import type { OperatorMonitoringSnapshot } from '../src/types.js';

const pageSource = await readFile(new URL('../src/operator/pages/OperatorPages.tsx', import.meta.url), 'utf8');
const componentSource = await readFile(new URL('../src/operator/MonitoringSnapshot.tsx', import.meta.url), 'utf8');
const i18nSource = await readFile(new URL('../src/i18n.tsx', import.meta.url), 'utf8');
const themeSource = await readFile(new URL('../src/theme.css', import.meta.url), 'utf8');

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

test('monitoring badges expose the current routing state without treating an unknown signal as healthy', () => {
  assert.match(componentSource, /t\('monitoring\.routingStatus'\)/);
  assert.match(componentSource, /if \(status === 'healthy'\) return 'ok';[\s\S]*if \(status === 'degraded'\) return 'pending';[\s\S]*return 'unknown';/);
  assert.match(i18nSource, /'monitoring\.routingStatus': 'Current routing status'/);
  assert.match(i18nSource, /'monitoring\.health\.healthy': 'Accepting traffic', 'monitoring\.health\.degraded': 'Awaiting recovery probe', 'monitoring\.health\.unhealthy': 'Unavailable', 'monitoring\.health\.unknown': 'No current signal'/);
  assert.match(i18nSource, /'monitoring\.health\.healthy': '可接收请求', 'monitoring\.health\.degraded': '等待恢复探测', 'monitoring\.health\.unhealthy': '暂不可用', 'monitoring\.health\.unknown': '未有当前信号'/);
});

test('monitoring freshness renders localized elapsed hours, minutes, and seconds without changing its timestamp evidence', () => {
  assert.equal(formatElapsedTime(5_534_801, 'en'), '1h 32m 14s');
  assert.equal(formatElapsedTime(5_534_801, 'zh-CN'), '1小时32分14秒');
  assert.equal(formatElapsedTime(999, 'en'), '0h 0m 0s');
  assert.match(componentSource, /formatElapsedTime\(freshness\.age_millis, locale\)/);
  assert.match(componentSource, /title=\{occurred\}/);
  assert.doesNotMatch(componentSource, /formatMilliseconds\(freshness\.age_millis, locale\)/);
});

test('light monitoring cards retain a light surface with legible upstream detail text', () => {
  assert.match(themeSource, /:root\[data-theme='light'\] \.operator-monitoring \.monitoring-top-list > li[\s\S]*background: #f8fbf9[\s\S]*border-color: #b9ceca/);
  assert.match(themeSource, /:root\[data-theme='light'\] \.operator-monitoring \.monitoring-top-heading code[\s\S]*color: #087d70/);
  assert.match(themeSource, /:root\[data-theme='light'\] \.operator-monitoring \.monitoring-metric-list dt,[\s\S]*color: #526970/);
});
