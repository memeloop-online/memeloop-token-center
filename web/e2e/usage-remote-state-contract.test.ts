import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import { defaultUsageSelection, localDateTimeInput, nextUsageTab, statsQuery, usageAstFromSelection, usageAstIssue, usageSelectionFromAst, usageSelectionFromDraft, usageRange, type UsageSelection } from '../src/operator/usageState.js';

const operatorSource = await readFile(new URL('../src/operator/UsageAnalysis.tsx', import.meta.url), 'utf8');
const usageStateSource = await readFile(new URL('../src/operator/usageState.ts', import.meta.url), 'utf8');
const builderSource = await readFile(new URL('../src/operator/TypedFilterBuilder.tsx', import.meta.url), 'utf8');
const pickerSource = await readFile(new URL('../src/operator/FilterCredentialAliasPicker.tsx', import.meta.url), 'utf8');
const projectionSource = await readFile(new URL('../src/operator/hooks/useUsageProjection.ts', import.meta.url), 'utf8');
const selfSource = await readFile(new URL('../src/self/UsagePage.tsx', import.meta.url), 'utf8');

test('operator data is scoped to the exact token, tenant, and applied selection', () => {
  assert.match(operatorSource, /const scope = useMemo\(\(\) => \(\{\}\), \[token, tenant, applied, refresh\]\)/);
  assert.match(projectionSource, /remote\?\.scope === scope/);
  for (const status of ['loading', 'error', 'ready']) assert.match(projectionSource, new RegExp(`status: '${status}'`));
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

test('usage filters share the request filter builder instead of a parallel form', () => {
  // The page contributes no filter form of its own: period, credential, and
  // conditions all live in the shared builder.
  assert.doesNotMatch(operatorSource, /panelControls|panelActive|UsageCredentialAliasSelect/);
  assert.doesNotMatch(operatorSource, /<details className="usage-filter-disclosure"/);
  assert.match(operatorSource, /scope="usage"/);
  assert.match(operatorSource, /usageSelection=\{selection\}/);
  assert.match(operatorSource, /externalChips=\{externalFilterChips\}/);
  assert.match(operatorSource, /onClear=\{\(\) => \{ const next = defaultUsageSelection\(\);/);
  // The credential alias picker is shared and reads paged aliases on demand.
  assert.match(builderSource, /FilterCredentialAliasPicker/);
  assert.match(pickerSource, /<Combobox[^>]+inlinePopup/);
  assert.match(pickerSource, /keyListPath\(tenant, cursor, \{ search \}\)/);
  assert.match(pickerSource, /keyListPage\(rows\)/);
  assert.match(pickerSource, /appendDistinctKeys/);
  assert.match(pickerSource, /credential\.alias/);
  assert.match(pickerSource, /new AbortController\(\)/);
});

test('builder drafts reconcile quick-range presets, manual edits, and the usage default period', () => {
  const now = Date.parse('2026-10-02T12:00:00Z');
  const base: UsageSelection = { ...defaultUsageSelection(now), preset: '7d', resolvedAt: now };
  const ast = usageAstFromSelection({ logical_operator: 'and', conditions: [] }, base);
  assert.ok(ast);
  // An untouched quick-range draft keeps its preset semantics on apply.
  const kept = usageSelectionFromDraft(base, ast, now);
  assert.equal(kept?.preset, '7d');
  assert.equal(kept?.resolvedAt, now);
  // A hand-edited time row becomes an explicit custom range.
  const edited = { ...ast, conditions: ast.conditions.map((condition) => condition.field === 'created_at' ? { ...condition, value: { type: 'timestamp' as const, value: now - 3_600_000 } } : condition) };
  const manual = usageSelectionFromDraft(base, edited, now);
  assert.equal(manual?.preset, 'custom');
  assert.equal(manual?.customFrom, localDateTimeInput(now - 3_600_000));
  // Usage queries cannot be open-ended: a missing period defaults to 24h.
  const noPeriod = { logical_operator: 'and' as const, conditions: ast.conditions.filter((condition) => condition.field !== 'created_at') };
  const defaulted = usageSelectionFromDraft(base, noPeriod, now);
  assert.equal(defaulted?.preset, '24h');
  assert.equal(defaulted?.resolvedAt, now);
  assert.equal(new URLSearchParams(statsQuery('', defaulted!)?.slice(1)).get('to_created_at'), String(now));
  // Unsupported or duplicate conditions still fail loudly instead of being dropped.
  const unsupported = { logical_operator: 'and' as const, conditions: [{ field: 'model' as const, operator: 'contains' as const, value: { type: 'model' as const, value: 'public-model' } }] };
  assert.equal(usageSelectionFromDraft(base, unsupported, now), undefined);
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

test('deleting the credential alias row clears the applied alias while keeping model and period semantics', () => {
  const now = Date.parse('2026-10-02T12:00:00Z');
  const base: UsageSelection = { ...defaultUsageSelection(now), preset: '7d', resolvedAt: now, filters: { ...defaultUsageSelection(now).filters, model: 'public-model', keyAlias: 'client-a' } };
  const ast = usageAstFromSelection({ logical_operator: 'and', conditions: [] }, base);
  assert.ok(ast);
  const pruned = { ...ast, conditions: ast.conditions.filter((condition) => condition.field !== 'key_alias') };
  // A full draft no longer resurrects the alias the user just deleted.
  const applied = usageSelectionFromDraft(base, pruned, now);
  assert.ok(applied);
  assert.equal(applied.filters.keyAlias, '');
  assert.equal(applied.filters.model, 'public-model');
  assert.equal(applied.preset, '7d');
  assert.equal(applied.resolvedAt, now);
  assert.equal(new URLSearchParams(statsQuery('', applied)?.slice(1)).get('key_alias'), null);
  // Reopening a saved full AST without the alias starts from an empty alias too.
  const reopened = usageSelectionFromAst(base, pruned);
  assert.equal(reopened?.filters.keyAlias, '');
  // The legacy draft path never carries key_alias rows, so it keeps the current alias.
  const legacy = usageSelectionFromAst(base, { logical_operator: 'and', conditions: [{ field: 'model', operator: 'equals', value: { type: 'model', value: 'public-model' } }] }, false);
  assert.equal(legacy?.filters.keyAlias, 'client-a');
  assert.equal(legacy?.filters.model, 'public-model');
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
