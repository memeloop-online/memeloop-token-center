export const requestConfigPurposes = ['general', 'codex', 'oauth', 'plugin', 'plugin_release', 'probe'] as const;
export type RequestConfigPurpose = (typeof requestConfigPurposes)[number];
export type RequestConfigField = 'connect_timeout_ms' | 'read_timeout_ms' | 'request_timeout_ms' | 'pool_idle_timeout_ms' | 'pool_max_idle_per_host';
export type HttpOverrides = Partial<Record<Exclude<RequestConfigField, 'pool_max_idle_per_host'>, number | null>> & { pool_max_idle_per_host?: number };
export type HttpSettings = { defaults: HttpOverrides; purposes: Partial<Record<RequestConfigPurpose, HttpOverrides>> };
export type RequestConfigSource = 'purpose_override' | 'global_default' | 'source_default';
export interface RequestConfigFieldMeta { key: RequestConfigField; unit: 'milliseconds' | 'connections'; nullable: boolean }
export const requestConfigFields: readonly RequestConfigFieldMeta[] = [
  { key: 'connect_timeout_ms', unit: 'milliseconds', nullable: true },
  { key: 'read_timeout_ms', unit: 'milliseconds', nullable: true },
  { key: 'request_timeout_ms', unit: 'milliseconds', nullable: true },
  { key: 'pool_idle_timeout_ms', unit: 'milliseconds', nullable: true },
  { key: 'pool_max_idle_per_host', unit: 'connections', nullable: false },
];
export interface RequestConfigEffectiveValue { value: number | null; source: RequestConfigSource; unit: 'milliseconds' | 'connections' }
export interface RequestConfigSnapshot {
  revision: number;
  settings: HttpSettings;
  effective: Record<RequestConfigPurpose, Record<RequestConfigField, RequestConfigEffectiveValue>>;
}
export function formatConfigValue(value: number | null, field: RequestConfigFieldMeta, unlimited: string, units: { milliseconds: string; connections: string }): string {
  return value === null ? unlimited : `${value} ${units[field.unit]}`;
}
export function toDisplayValue(value: number | null | undefined): string {
  return value === undefined ? '' : value === null ? 'off' : String(value);
}
export type ParsedConfigInput = { ok: true; value: number | null | undefined } | { ok: false };
export function parseConfigInput(text: string, field: RequestConfigFieldMeta): ParsedConfigInput {
  const trimmed = text.trim();
  if (!trimmed) return { ok: true, value: undefined };
  if (trimmed === 'off') return field.nullable ? { ok: true, value: null } : { ok: false };
  if (!/^\d+$/.test(trimmed)) return { ok: false };
  const value = Number(trimmed);
  return Number.isSafeInteger(value) && value >= (field.nullable ? 1 : 0) ? { ok: true, value } : { ok: false };
}
