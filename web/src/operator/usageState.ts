import type { TypedFilterAst, TypedFilterCondition } from '../types.js';

export type UsageTab = 'overview' | 'trend' | 'dimensions' | 'heatmap';
export type Preset = '24h' | 'today' | 'yesterday' | '7d' | '30d' | 'custom';
export type Granularity = 'auto' | 'hour' | 'day';
export interface UsageFilters { model: string; keyId: string; keyAlias: string; upstreamId: string; protocol: string; status: string; errorCode: string }
export interface UsageSelection { preset: Preset; granularity: Granularity; customFrom: string; customTo: string; resolvedAt?: number; filters: UsageFilters }
type UsageQuerySelection = Omit<UsageSelection, 'filters'> & { filters: Omit<UsageFilters, 'keyAlias'> & Partial<Pick<UsageFilters, 'keyAlias'>> };

export const usageTabs: UsageTab[] = ['overview', 'trend', 'dimensions', 'heatmap'];
export const usagePresets: Preset[] = ['24h', 'today', 'yesterday', '7d', '30d', 'custom'];
export const emptyUsageFilters: UsageFilters = { model: '', keyId: '', keyAlias: '', upstreamId: '', protocol: '', status: '', errorCode: '' };

export function nextUsageTab(current: UsageTab, key: string) {
  const index = usageTabs.indexOf(current);
  if (key === 'Home') return usageTabs[0];
  if (key === 'End') return usageTabs[usageTabs.length - 1];
  if (key === 'ArrowRight') return usageTabs[(index + 1) % usageTabs.length];
  if (key === 'ArrowLeft') return usageTabs[(index - 1 + usageTabs.length) % usageTabs.length];
  return undefined;
}

export function localDateTimeInput(epoch: number, precision: 'minute' | 'milliseconds' = 'milliseconds') {
  const date = new Date(epoch);
  return new Date(epoch - date.getTimezoneOffset() * 60_000).toISOString().slice(0, precision === 'minute' ? 16 : 23);
}

export function defaultUsageSelection(now = Date.now()): UsageSelection {
  return {
    preset: '24h',
    granularity: 'auto',
    customFrom: localDateTimeInput(now - 86_400_000),
    customTo: localDateTimeInput(now),
    resolvedAt: now,
    filters: { ...emptyUsageFilters },
  };
}

export function usageRange(selection: Pick<UsageSelection, 'preset' | 'customFrom' | 'customTo' | 'resolvedAt'>) {
  const end = selection.resolvedAt ?? Date.now();
  if (selection.preset === '24h') return { from: end - 86_400_000, to: end };
  if (selection.preset === '7d') return { from: end - 7 * 86_400_000, to: end };
  if (selection.preset === '30d') return { from: end - 30 * 86_400_000, to: end };
  const today = new Date(end); today.setHours(0, 0, 0, 0);
  if (selection.preset === 'today') return { from: today.getTime(), to: end };
  if (selection.preset === 'yesterday') { const yesterday = new Date(today); yesterday.setDate(yesterday.getDate() - 1); return { from: yesterday.getTime(), to: today.getTime() - 1 }; }
  const from = Date.parse(selection.customFrom); const to = Date.parse(selection.customTo);
  if (!Number.isFinite(from) || !Number.isFinite(to) || from > to) return undefined;
  return { from, to };
}

export function usageAstIssue(ast: TypedFilterAst): 'duplicate' | 'unsupported' | undefined {
  if (ast.logical_operator !== 'and') return 'unsupported';
  const expectedTypes: Partial<Record<TypedFilterCondition['field'], TypedFilterCondition['value']['type']>> = { created_at: 'timestamp', model: 'model', key_id: 'uuid', key_alias: 'text', upstream_account_id: 'uuid', protocol: 'protocol', status: 'status', error_code: 'text' };
  const seen = new Set<TypedFilterCondition['field']>();
  for (const condition of ast.conditions) {
    if (seen.has(condition.field)) return 'duplicate';
    seen.add(condition.field);
    const expectedType = expectedTypes[condition.field];
    if (!expectedType || condition.value.type !== expectedType || (condition.field === 'created_at' ? condition.operator !== 'between' || condition.upper?.type !== 'timestamp' : condition.operator !== 'equals' || condition.upper !== undefined)) return 'unsupported';
    if (typeof condition.value.value === 'string' && !condition.value.value.trim()) return 'unsupported';
    if (condition.field === 'created_at') { const lower = Number(condition.value.value); const upper = Number(condition.upper?.value); if (!Number.isFinite(lower) || !Number.isFinite(upper) || lower > upper) return 'unsupported'; }
  }
  return undefined;
}

export function usageSelectionFromAst(current: UsageSelection, ast: TypedFilterAst, readPeriod = true): UsageSelection | undefined {
  if (usageAstIssue(ast) || (!readPeriod && ast.conditions.some((condition) => condition.field === 'created_at' || condition.field === 'key_alias'))) return undefined;
  const filters = { ...emptyUsageFilters, keyAlias: current.filters.keyAlias, upstreamId: current.filters.upstreamId === 'unassigned' ? 'unassigned' : '' };
  let period = { preset: current.preset, customFrom: current.customFrom, customTo: current.customTo };
  for (const condition of ast.conditions) {
    if (readPeriod && condition.field === 'created_at' && condition.operator === 'between' && condition.value.type === 'timestamp' && condition.upper?.type === 'timestamp') {
      period = { preset: 'custom', customFrom: localDateTimeInput(condition.value.value), customTo: localDateTimeInput(condition.upper.value) };
    } else if (condition.operator === 'equals' && condition.field === 'model' && condition.value.type === 'model') filters.model = condition.value.value;
    else if (condition.operator === 'equals' && condition.field === 'key_id' && condition.value.type === 'uuid') filters.keyId = condition.value.value;
    else if (condition.operator === 'equals' && condition.field === 'key_alias' && condition.value.type === 'text') filters.keyAlias = condition.value.value;
    else if (condition.operator === 'equals' && condition.field === 'upstream_account_id' && condition.value.type === 'uuid') filters.upstreamId = condition.value.value;
    else if (condition.operator === 'equals' && condition.field === 'protocol' && condition.value.type === 'protocol') filters.protocol = condition.value.value;
    else if (condition.operator === 'equals' && condition.field === 'status' && condition.value.type === 'status') filters.status = condition.value.value;
    else if (condition.operator === 'equals' && condition.field === 'error_code' && condition.value.type === 'text') filters.errorCode = condition.value.value;
  }
  return { ...current, ...period, filters };
}

export function usageAstFromSelection(ast: TypedFilterAst, selection: UsageSelection): TypedFilterAst | undefined {
  if (usageAstIssue(ast)) return undefined;
  const range = usageRange(selection);
  if (!range) return undefined;
  const filters = selection.filters;
  const conditions: TypedFilterCondition[] = [
    { field: 'created_at', operator: 'between', value: { type: 'timestamp', value: range.from }, upper: { type: 'timestamp', value: range.to } },
  ];
  if (filters.model.trim()) conditions.push({ field: 'model', operator: 'equals', value: { type: 'model', value: filters.model.trim() } });
  if (filters.keyId.trim()) conditions.push({ field: 'key_id', operator: 'equals', value: { type: 'uuid', value: filters.keyId.trim() } });
  if (filters.keyAlias.trim()) conditions.push({ field: 'key_alias', operator: 'equals', value: { type: 'text', value: filters.keyAlias.trim() } });
  if (filters.upstreamId && filters.upstreamId !== 'unassigned') conditions.push({ field: 'upstream_account_id', operator: 'equals', value: { type: 'uuid', value: filters.upstreamId } });
  if (filters.protocol) conditions.push({ field: 'protocol', operator: 'equals', value: { type: 'protocol', value: filters.protocol as 'openai' | 'anthropic' | 'openai-image' | 'audio-transcription' | 'generation' } });
  if (filters.status === 'success' || filters.status === 'error' || filters.status === 'pending') conditions.push({ field: 'status', operator: 'equals', value: { type: 'status', value: filters.status } });
  if (filters.errorCode.trim()) conditions.push({ field: 'error_code', operator: 'equals', value: { type: 'text', value: filters.errorCode.trim() } });
  for (const source of ast.conditions) {
    if (source.field === 'created_at') continue;
    const normalized = conditions.find((condition) => condition.field === source.field);
    if (!normalized || normalized.operator !== source.operator || normalized.value.type !== source.value.type || normalized.value.value !== source.value.value) return undefined;
  }
  return { logical_operator: 'and', conditions };
}

export function statsQuery(tenant: string, selection: UsageQuerySelection) {
  const range = usageRange(selection); if (!range) return undefined;
  const params = new URLSearchParams({ from_created_at: String(range.from), to_created_at: String(range.to), granularity: selection.granularity });
  if (tenant) params.set('tenant_external_id', tenant);
  if (selection.filters.model.trim()) params.set('model', selection.filters.model.trim());
  if (selection.filters.keyId.trim()) params.set('key_id', selection.filters.keyId.trim());
  const keyAlias = selection.filters.keyAlias?.trim();
  if (keyAlias) params.set('key_alias', keyAlias);
  if (selection.filters.upstreamId) params.set('upstream_account_id', selection.filters.upstreamId);
  if (selection.filters.protocol) params.set('protocol', selection.filters.protocol);
  if (selection.filters.status) params.set('status', selection.filters.status);
  if (selection.filters.errorCode.trim()) params.set('error_code', selection.filters.errorCode.trim());
  return `?${params}`;
}
