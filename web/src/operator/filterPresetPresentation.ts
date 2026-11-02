import type { TypedFilterAst, TypedFilterCondition, TypedFilterValue, UpstreamAccount } from '../types.js';

type Translate = (key: string) => string;
type AccountName = Pick<UpstreamAccount, 'id' | 'name'>;

function displayValue(value: TypedFilterValue, field: TypedFilterCondition['field'], locale: string, t: Translate, accounts: AccountName[], compact: boolean): string {
  if (value.type === 'timestamp') {
    const date = new Date(value.value);
    if (!Number.isFinite(date.getTime())) return String(value.value);
    return new Intl.DateTimeFormat(locale, compact
      ? { month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', hourCycle: 'h23' }
      : { dateStyle: 'short', timeStyle: 'short' }).format(date);
  }
  if (value.type === 'status') return t(value.value === 'success' ? 'traffic.success' : value.value === 'error' ? 'traffic.failure' : 'common.running');
  if (field === 'session_association') return t(`filter.association.${value.value}`);
  if (field === 'upstream_account_id') return accounts.find((account) => account.id === value.value)?.name ?? String(value.value);
  return String(value.value);
}

export function filterConditionLabel(condition: TypedFilterCondition, locale: string, t: Translate, accounts: AccountName[] = [], compact = false): string {
  const value = displayValue(condition.value, condition.field, locale, t, accounts, compact);
  const upper = condition.upper ? displayValue(condition.upper, condition.field, locale, t, accounts, compact) : undefined;
  if (compact && condition.operator === 'equals' && (condition.field === 'model' || condition.field === 'status')) return value;
  return `${t(`filter.field.${condition.field}`)} ${t(`filter.operator.${condition.operator}`)} ${value}${upper === undefined ? '' : ` – ${upper}`}`;
}

/** Keep model and outcome visible even when the saved AST starts with a date range. */
export function filterPresetPresentation(ast: TypedFilterAst, locale: string, t: Translate, accounts: AccountName[] = []): { summary: string; detail: string } {
  if (!ast.conditions.length) return { summary: t('filter.noneApplied'), detail: t('filter.noneApplied') };
  const priority = (condition: TypedFilterCondition) => condition.field === 'model' ? 0 : condition.field === 'status' ? 1 : condition.field === 'created_at' ? 3 : 2;
  const conditions = [...ast.conditions].sort((a, b) => priority(a) - priority(b));
  const parts = conditions.slice(0, 3).map((condition) => filterConditionLabel(condition, locale, t, accounts, true));
  if (conditions.length > 3) parts.push(t('filter.moreConditions').replace('{{count}}', String(conditions.length - 3)));
  const summary = parts.join(' · ');
  return {
    summary: summary.length > 120 ? `${summary.slice(0, 119)}…` : summary,
    detail: ast.conditions.map((condition) => filterConditionLabel(condition, locale, t, accounts)).join(' · '),
  };
}
