import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import { nextUsageTab, statsQuery, type UsageSelection } from '../src/operator/usageState.js';

const operatorSource = await readFile(new URL('../src/operator/UsageAnalysis.tsx', import.meta.url), 'utf8');
const selfSource = await readFile(new URL('../src/self/UsagePage.tsx', import.meta.url), 'utf8');

test('operator data is scoped to the exact token, tenant, and applied selection', () => {
  assert.match(operatorSource, /const scope = useMemo\(\(\) => \(\{\}\), \[token, tenant, applied, refresh\]\)/);
  assert.match(operatorSource, /remote\?\.scope === scope/);
  for (const status of ['loading', 'error', 'ready']) assert.match(operatorSource, new RegExp(`status: '${status}'`));
});

test('refresh re-fetches applied filters without silently applying the draft model', () => {
  const applied: UsageSelection = {
    preset: 'custom', granularity: 'hour', customFrom: '2026-08-29T00:00', customTo: '2026-08-30T00:00',
    filters: { model: 'applied-model', keyId: '', upstreamId: '', protocol: '', status: '', errorCode: '' },
  };
  const draft = { ...applied, filters: { ...applied.filters, model: 'unsubmitted-draft' } };
  const refreshQuery = statsQuery('tenant-a', applied);
  assert.match(refreshQuery ?? '', /model=applied-model/);
  assert.doesNotMatch(refreshQuery ?? '', /unsubmitted-draft/);
  assert.equal(draft.filters.model, 'unsubmitted-draft');
  assert.match(operatorSource, /onClick=\{\(\) => setRefresh\(\(value\) => value \+ 1\)\}/);
});

test('usage tabs implement roving keyboard focus and linked tab panels', () => {
  assert.equal(nextUsageTab('overview', 'ArrowLeft'), 'heatmap');
  assert.equal(nextUsageTab('heatmap', 'ArrowRight'), 'overview');
  assert.equal(nextUsageTab('trend', 'Home'), 'overview');
  assert.equal(nextUsageTab('trend', 'End'), 'heatmap');
  assert.equal(nextUsageTab('trend', 'Enter'), undefined);
  assert.match(operatorSource, /aria-controls=\{`usage-panel-\$\{id\}`\}/);
  assert.match(operatorSource, /tabIndex=\{tab === id \? 0 : -1\}/);
  assert.match(operatorSource, /aria-labelledby=\{`usage-tab-\$\{tab\}`\}/);
});

test('self usage range changes cannot render the previous response', () => {
  assert.match(selfSource, /const scope = useMemo\(\(\) => \(\{\}\), \[credential, range, refresh\]\)/);
  assert.match(selfSource, /remote\?\.scope === scope/);
  assert.match(selfSource, /t\('self\.usageDescription'\)/);
  assert.match(selfSource, /role="alert"/);
});

test('timezone and keyboard drilldown are shared by charts and equivalent tables', () => {
  assert.match(operatorSource, /toLocaleString\([^\n]+\{ timeZone \}/);
  assert.match(operatorSource, /onSelect=\{selectUtcBucket\}/);
  assert.match(operatorSource, /setSelectedHeatHour\(value\.hour_of_week\)/);
  assert.match(selfSource, /toLocaleString\(locale, \{ timeZone \}\)/);
  assert.match(selfSource, /timeZone=\{timeZone\}/);
});

test('heatmap selection uses the stable hour identity instead of response order', () => {
  assert.match(operatorSource, /find\(\(value\) => value\.hour_of_week === selectedHeatHour\)/);
  assert.match(operatorSource, /stats\.heatmap\[dataIndex\]\?\.hour_of_week/);
  assert.doesNotMatch(operatorSource, /stats\.heatmap\[selectedHeatCell\]/);
});
