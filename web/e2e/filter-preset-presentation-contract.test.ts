import assert from 'node:assert/strict';
import test from 'node:test';
import { filterConditionLabel, filterPresetPresentation } from '../src/operator/filterPresetPresentation.js';
import type { TypedFilterAst } from '../src/types.js';

const messages: Record<string, string> = {
  'filter.field.model': 'Model', 'filter.field.status': 'Status', 'filter.field.created_at': 'Time',
  'filter.field.upstream_account_id': 'Account', 'filter.field.error_code': 'Error',
  'filter.operator.equals': 'equals', 'filter.operator.contains': 'contains', 'filter.operator.between': 'between',
  'traffic.failure': 'Failed', 'traffic.success': 'Success', 'common.running': 'Running',
  'filter.moreConditions': '{{count}} more', 'filter.noneApplied': 'No filters applied',
};
const t = (key: string) => messages[key] ?? key;
const ast: TypedFilterAst = { logical_operator: 'and', conditions: [
  { field: 'created_at', operator: 'between', value: { type: 'timestamp', value: Date.UTC(2026, 9, 15) }, upper: { type: 'timestamp', value: Date.UTC(2026, 9, 16) } },
  { field: 'status', operator: 'equals', value: { type: 'status', value: 'error' } },
  { field: 'model', operator: 'equals', value: { type: 'model', value: 'gpt-6-astra' } },
] };

test('saved filter previews prioritize model and translated outcome, preserving full range in details', () => {
  const preview = filterPresetPresentation(ast, 'en', t);
  assert.ok(preview.summary.startsWith('gpt-6-astra · Failed · Time between'));
  assert.ok(preview.summary.length <= 120);
  assert.match(preview.detail, /Model equals gpt-6-astra/);
  assert.match(preview.detail, /Status equals Failed/);
  assert.ok(preview.detail.includes(new Intl.DateTimeFormat('en', { dateStyle: 'short', timeStyle: 'short' }).format(Date.UTC(2026, 9, 15))));
  assert.equal(ast.conditions[0].field, 'created_at', 'rendering must not reorder the stored AST');
  const zh = (key: string) => key === 'traffic.failure' ? '失败' : t(key);
  assert.match(filterPresetPresentation(ast, 'zh-CN', zh).summary, /gpt-6-astra · 失败/);
});

test('previews resolve account names and retain long and omitted conditions in accessible details', () => {
  const longName = 'long-model-'.repeat(40);
  const many: TypedFilterAst = { logical_operator: 'and', conditions: [
    ...ast.conditions,
    { field: 'upstream_account_id', operator: 'equals', value: { type: 'uuid', value: 'account-id' } },
    { field: 'error_code', operator: 'contains', value: { type: 'text', value: 'http2' } },
  ] };
  const accounts = [{ id: 'account-id', name: 'Historical account' }];
  const preview = filterPresetPresentation(many, 'en', t, accounts);
  assert.match(preview.summary, /2 more/);
  assert.match(preview.detail, /Account equals Historical account/);
  const longAst: TypedFilterAst = { logical_operator: 'and', conditions: [{ field: 'model', operator: 'equals', value: { type: 'model', value: longName } }] };
  const long = filterPresetPresentation(longAst, 'en', t);
  assert.equal(long.summary.length, 120);
  assert.ok(long.summary.endsWith('…'));
  assert.ok(long.detail.includes(longName));
  assert.match(filterConditionLabel({ field: 'model', operator: 'contains', value: { type: 'model', value: 'astra' } }, 'en', t), /contains astra/);
  assert.equal(filterPresetPresentation({ logical_operator: 'and', conditions: [] }, 'en', t).summary, 'No filters applied');
});
