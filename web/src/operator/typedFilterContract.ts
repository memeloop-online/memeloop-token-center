import type { TypedFilterAst, TypedFilterCondition, TypedFilterField, TypedFilterOperator, TypedFilterValue } from '../types.js';

export type BuilderScope = 'requests' | 'usage';
export type ValueType = TypedFilterValue['type'];

export interface FieldDefinition {
  id: TypedFilterField;
  type: ValueType;
  usage: boolean;
}

export const fields: FieldDefinition[] = [
  { id: 'created_at', type: 'timestamp', usage: true },
  { id: 'model', type: 'model', usage: true },
  { id: 'key_id', type: 'uuid', usage: true },
  { id: 'upstream_account_id', type: 'uuid', usage: true },
  { id: 'protocol', type: 'protocol', usage: true },
  { id: 'status', type: 'status', usage: true },
  { id: 'error_code', type: 'text', usage: true },
  { id: 'route_id', type: 'uuid', usage: false },
  { id: 'duration_ms', type: 'integer', usage: false },
  { id: 'cost_micros', type: 'money_micros', usage: false },
  { id: 'key_alias', type: 'text', usage: true },
  { id: 'principal', type: 'text', usage: false },
  { id: 'session_association', type: 'text', usage: false },
  { id: 'session_id', type: 'text', usage: false },
  { id: 'client_session_id', type: 'text', usage: false },
];

export const numericOperators: TypedFilterOperator[] = [
  'equals', 'not_equals', 'greater_than', 'greater_than_or_equal', 'less_than', 'less_than_or_equal', 'between',
];
export const textOperators: TypedFilterOperator[] = ['equals', 'not_equals', 'contains'];
export const exactOperators: TypedFilterOperator[] = ['equals', 'not_equals'];

export function operatorsFor(field: FieldDefinition, scope: BuilderScope): TypedFilterOperator[] {
  if (scope === 'usage') return field.id === 'created_at' ? ['between'] : ['equals'];
  if (field.id === 'session_association' || field.id === 'session_id' || field.id === 'client_session_id') return exactOperators;
  if (field.type === 'timestamp' || field.type === 'integer' || field.type === 'money_micros') return numericOperators;
  if (field.type === 'text' || field.type === 'model') return textOperators;
  return exactOperators;
}

function record(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function decodeValue(value: unknown, type: ValueType): TypedFilterValue | undefined {
  if (!record(value) || value.type !== type) return undefined;
  if (type === 'integer' || type === 'timestamp' || type === 'money_micros') {
    return typeof value.value === 'number' && Number.isFinite(value.value) ? { type, value: value.value } : undefined;
  }
  if (typeof value.value !== 'string') return undefined;
  if (type === 'protocol') {
    const protocol = value.value;
    return protocol === 'openai' || protocol === 'anthropic' || protocol === 'openai-image' || protocol === 'audio-transcription' || protocol === 'generation'
      ? { type, value: protocol } : undefined;
  }
  if (type === 'status') {
    const status = value.value;
    return status === 'success' || status === 'error' || status === 'pending' ? { type, value: status } : undefined;
  }
  return { type, value: value.value };
}

/** Decode the shared wire shape; field/operator semantics remain server-owned. */
export function decodeTypedFilterAst(value: unknown): TypedFilterAst | undefined {
  if (!record(value) || value.logical_operator !== 'and' || !Array.isArray(value.conditions)) return undefined;
  const conditions: TypedFilterCondition[] = [];
  for (const candidate of value.conditions) {
    if (!record(candidate)) return undefined;
    const field = fields.find(item => item.id === candidate.field);
    const operator = numericOperators.find(item => item === candidate.operator)
      ?? textOperators.find(item => item === candidate.operator);
    if (!field || !operator) return undefined;
    const decoded = decodeValue(candidate.value, field.type);
    if (!decoded) return undefined;
    const upper = candidate.upper === undefined ? undefined : decodeValue(candidate.upper, field.type);
    if ((candidate.upper !== undefined && !upper) || (operator === 'between' && !upper)) return undefined;
    conditions.push({ field: field.id, operator, value: decoded, ...(upper ? { upper } : {}) });
  }
  return { logical_operator: 'and', conditions };
}
