import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import { defaultUsageSelection, localDateTimeInput, nextUsageTab, statsQuery, usageAstFromSelection, usageAstIssue, usageSelectionFromAst, usageRange, type UsageSelection } from '../src/operator/usageState.js';

const operatorSource = await readFile(new URL('../src/operator/UsageAnalysis.tsx', import.meta.url), 'utf8');
const usageStateSource = await readFile(new URL('../src/operator/usageState.ts', import.meta.url), 'utf8');
const selfSource = await readFile(new URL('../src/self/UsagePage.tsx', import.meta.url), 'utf8');

test('operator data is scoped to the exact token, tenant, and applied selection', () => {
  assert.match(operatorSource, /const scope = useMemo\(\(\) => \(\{\}\), \[token, tenant, applied, refresh\]\)/);
  assert.match(operatorSource, /remote\?\.scope === scope/);
  for (const status of ['loading', 'error', 'ready']) assert.match(operatorSource, new RegExp(`status: '${status}'`));
});

test('refresh re-fetches applied filters without silently applying the draft model', () => {
  const applied: UsageSelection = {
    preset: 'custom', granularity: 'hour', customFrom: '2026-08-29T00:00', customTo: '2026-08-30T00:00',
    filters: { model: 'applied-model', keyId: '', keyAlias: '', upstreamId: '', protocol: '', status: '', errorCode: '' },
  };
  const draft = { ...applied, filters: { ...applied.filters, model: 'unsubmitted-draft' } };
  const refreshQuery = statsQuery('tenant-a', applied);
  assert.match(refreshQuery ?? '', /model=applied-model/);
  assert.doesNotMatch(refreshQuery ?? '', /unsubmitted-draft/);
  assert.equal(draft.filters.model, 'unsubmitted-draft');
  assert.match(operatorSource, /onClick=\{refreshUsage\}/);
  assert.match(operatorSource, /setTypedFilters\(usageAstFromSelection\(emptyTypedFilterAst, next\)!\); setRefresh/);
});

test('usage key alias filters are exposed and serialized', () => {
  const query = statsQuery('tenant-a', {
    preset: '24h', granularity: 'hour', customFrom: '', customTo: '',
    filters: { model: '', keyId: '', keyAlias: 'primary-key', upstreamId: '', protocol: '', status: '', errorCode: '' },
  });
  assert.equal(new URLSearchParams(query?.slice(1)).get('key_alias'), 'primary-key');
  assert.match(usageStateSource, /key_alias/);
});

test('usage filters use one shared panel with presets, explicit ranges, and credential choices', () => {
  assert.match(operatorSource, /<TypedFilterBuilder[^>]+panelControls=/);
  assert.doesNotMatch(operatorSource, /<details className="usage-filter-disclosure"/);
  assert.match(operatorSource, /usagePresets\.map/);
  assert.match(operatorSource, /UsageCredentialAliasSelect/);
  assert.match(operatorSource, /internal\/v1\/keys\?\$\{params\}/);
  assert.match(operatorSource, /credential\.alias/);
  assert.match(operatorSource, /onClear=\{\(\) => \{ const next = defaultUsageSelection\(\);/);
});

test('preset and custom periods serialize to the same AST and statistics range', () => {
  const now = Date.parse('2026-10-02T12:00:00Z');
  const preset = { ...defaultUsageSelection(now), preset: '7d' as const, filters: { ...defaultUsageSelection(now).filters, keyAlias: 'client-a' } };
  const ast = usageAstFromSelection({ logical_operator: 'and', conditions: [] }, preset);
  assert.ok(ast);
  const period = ast.conditions.find((condition) => condition.field === 'created_at');
  assert.equal(period?.value.value, usageRange(preset)?.from);
  assert.equal(period?.upper?.value, usageRange(preset)?.to);
  const query = new URLSearchParams(statsQuery('tenant-a', preset)?.slice(1));
  assert.equal(query.get('from_created_at'), String(period?.value.value));
  assert.equal(query.get('to_created_at'), String(period?.upper?.value));
  assert.equal(query.get('key_alias'), 'client-a');
  const reopened = usageSelectionFromAst(defaultUsageSelection(now), ast);
  assert.ok(reopened);
  assert.equal(reopened.preset, 'custom');
  assert.equal(statsQuery('tenant-a', reopened)?.includes(`from_created_at=${period?.value.value}`), true);
  assert.equal(reopened.filters.keyAlias, 'client-a');
  const custom = { ...reopened, customTo: localDateTimeInput(now - 8 * 86_400_000) };
  assert.equal(usageAstFromSelection(ast, custom), undefined);
});

test('typed filters normalize independently of the chosen period', () => {
  const selection = defaultUsageSelection(Date.parse('2026-10-02T12:00:00Z'));
  const ast = { logical_operator: 'and' as const, conditions: [
    { field: 'model' as const, operator: 'equals' as const, value: { type: 'model' as const, value: 'public-model' } },
  ] };
  const next = usageSelectionFromAst(selection, ast, false);
  assert.ok(next);
  assert.equal(next.preset, '24h');
  assert.equal(next.filters.model, 'public-model');
  assert.equal(new URLSearchParams(statsQuery('', next)?.slice(1)).get('model'), 'public-model');
  assert.equal(usageAstFromSelection(ast, next)?.conditions.filter((condition) => condition.field === 'created_at').length, 1);
});

test('usage conversion rejects duplicate or unrepresentable saved predicates', () => {
  const selection = defaultUsageSelection(Date.parse('2026-10-02T12:00:00Z'));
  const model = { field: 'model' as const, operator: 'equals' as const, value: { type: 'model' as const, value: 'public-model' } };
  const duplicate = { logical_operator: 'and' as const, conditions: [model, model] };
  const unsupported = { logical_operator: 'and' as const, conditions: [{ ...model, operator: 'contains' as const }] };
  assert.equal(usageAstIssue(duplicate), 'duplicate');
  assert.equal(usageSelectionFromAst(selection, duplicate), undefined);
  assert.equal(usageAstFromSelection(duplicate, selection), undefined);
  assert.equal(usageAstIssue(unsupported), 'unsupported');
  assert.equal(usageSelectionFromAst(selection, unsupported), undefined);
  assert.equal(usageAstFromSelection(unsupported, selection), undefined);
  assert.equal(usageAstFromSelection({ logical_operator: 'and', conditions: [model] }, selection), undefined);
  assert.equal(usageAstFromSelection({ logical_operator: 'and', conditions: [{ field: 'key_alias', operator: 'equals', value: { type: 'text', value: 'saved-alias' } }] }, selection), undefined);
});

test('shared local date conversion preserves milliseconds and supports minute inputs', () => {
  const epoch = Date.parse('2026-09-30T12:34:56.789Z');
  assert.equal(localDateTimeInput(epoch, 'milliseconds'), '2026-09-30T12:34:56.789');
  assert.equal(localDateTimeInput(epoch, 'minute'), '2026-09-30T12:34');
  assert.equal(defaultUsageSelection(epoch).preset, '24h');
  assert.equal(defaultUsageSelection(epoch).granularity, 'auto');
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
