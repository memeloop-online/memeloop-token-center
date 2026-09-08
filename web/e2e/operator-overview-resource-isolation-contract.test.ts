import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const pageSource = await readFile(new URL('../src/operator/pages/OperatorPages.tsx', import.meta.url), 'utf8');
const overviewSource = pageSource.slice(pageSource.indexOf('export function OverviewPage'), pageSource.indexOf('export function UsagePage'));
const usageSource = pageSource.slice(pageSource.indexOf('export function UsagePage'), pageSource.indexOf('export function GenerationsPage'));

test('Overview loads monitoring and recent requests as independent tenant-scoped resources', () => {
  assert.doesNotMatch(overviewSource, /Promise\.all/);
  assert.match(overviewSource, /const requestResource = useOperatorResource\([\s\S]*api<RequestView\[\]>\(`\/internal\/v1\/requests/);
  assert.match(overviewSource, /const monitoringResource = useOperatorResource\([\s\S]*api<OperatorMonitoringSnapshot>\(monitoringSnapshotPath/);
  assert.equal((overviewSource.match(/Boolean\(token\), `\$\{token\}\\0\$\{tenant\}`/g) ?? []).length, 2);
  assert.match(overviewSource, /<OverviewMonitoringSection state=\{monitoringResource\.state\}/);
  assert.match(overviewSource, /<OverviewRecentRequestsSection state=\{requestResource\.state\}/);
});

test('each Overview section reports its own state while a ready value remains visible on refresh errors', () => {
  const monitoringSection = pageSource.slice(pageSource.indexOf('function OverviewMonitoringSection'), pageSource.indexOf('function OverviewRecentRequestsSection'));
  const requestsSection = pageSource.slice(pageSource.indexOf('function OverviewRecentRequestsSection'), pageSource.indexOf('export function OverviewPage'));
  for (const section of [monitoringSection, requestsSection]) {
    assert.match(section, /state\.kind === 'idle' \|\| state\.kind === 'loading'/);
    assert.match(section, /state\.kind === 'failed'/);
    assert.match(section, /role="alert">\{state\.message\}/);
    assert.match(section, /state\.refreshError/);
  }
  assert.match(monitoringSection, /<MonitoringSnapshot snapshot=\{state\.value\}/);
  assert.match(requestsSection, /<RequestTable requests=\{state\.value\}/);
});

test('Usage statistics stay mounted when upstream filter metadata is loading or fails', () => {
  assert.match(usageSource, /const upstreams = resource\.state\.kind === 'ready' \? resource\.state\.value : \[\]/);
  assert.match(usageSource, /const upstreamMetadataError = resource\.state\.kind === 'failed'/);
  assert.match(usageSource, /role="alert">\{t\('usage\.upstreams'\)\}: \{upstreamMetadataError\}/);
  assert.match(usageSource, /<UsageAnalysis token=\{token\} tenant=\{tenant\} upstreams=\{upstreams\}/);
  assert.doesNotMatch(usageSource, /return <div className="empty">\{t\('common\.loading'\)\}<\/div>/);
});
