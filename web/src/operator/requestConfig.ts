import type { Locale } from '../i18n.js';

/**
 * Presentation metadata for the request-configuration settings. Wire details
 * (endpoint, payload, contract version) live only in requestConfigApi.ts and
 * follow the backend settings contract; this registry owns labels, units and
 * validation ranges so the page never displays raw field keys.
 */
export type RequestConfigUnit = 'milliseconds' | 'bytes' | 'count';
export type RequestConfigSource = 'default' | 'override';

export interface RequestConfigFieldMeta {
  key: string;
  unit: RequestConfigUnit;
  /** Canonical values are milliseconds / bytes; inputs use friendlier units. */
  inputScale: number;
  min: number;
  max: number;
  /** A nullable field left blank means the platform adds no extra limit. */
  nullable: boolean;
}

export const requestConfigFields: readonly RequestConfigFieldMeta[] = [
  { key: 'timeout_ms', unit: 'milliseconds', inputScale: 1_000, min: 1_000, max: 600_000, nullable: false },
  { key: 'stream_idle_timeout_ms', unit: 'milliseconds', inputScale: 1_000, min: 1_000, max: 300_000, nullable: true },
  { key: 'max_request_bytes', unit: 'bytes', inputScale: 1_048_576, min: 1_048_576, max: 268_435_456, nullable: true },
];

export const requestConfigPurposes = ['text', 'embedding', 'image', 'audio', 'video'] as const;
export type RequestConfigPurpose = (typeof requestConfigPurposes)[number];

export interface RequestConfigEffectiveValue { value: number | null; source: RequestConfigSource }
export interface RequestConfigPurposeConfig {
  overrides: Record<string, number>;
  effective: Record<string, RequestConfigEffectiveValue>;
}
export interface RequestConfigSnapshot {
  contract_version: string;
  defaults: Record<string, number | null>;
  purposes: Record<string, RequestConfigPurposeConfig>;
  updated_at: number;
}

const numberFormatters = new Map<string, Intl.NumberFormat>();
function numberFormatter(locale: Locale) {
  let formatter = numberFormatters.get(locale);
  if (!formatter) {
    formatter = new Intl.NumberFormat(locale, { maximumFractionDigits: 3, useGrouping: false });
    numberFormatters.set(locale, formatter);
  }
  return formatter;
}

/** User-facing value with an explicit unit; null stays visible as "no extra limit". */
export function formatConfigValue(
  value: number | null,
  meta: RequestConfigFieldMeta,
  locale: Locale,
  unlimitedLabel: string,
  units: { seconds: string; mebibytes: string },
): string {
  if (value === null || !Number.isFinite(value)) return unlimitedLabel;
  const amount = numberFormatter(locale).format(value / meta.inputScale);
  return meta.unit === 'milliseconds' ? `${amount} ${units.seconds}` : meta.unit === 'bytes' ? `${amount} ${units.mebibytes}` : amount;
}

export function toDisplayValue(value: number | null | undefined, meta: RequestConfigFieldMeta, locale: Locale): string {
  if (value === null || value === undefined || !Number.isFinite(value)) return '';
  return numberFormatter(locale).format(value / meta.inputScale);
}

export type ParsedConfigInput = { ok: true; value: number | null } | { ok: false };

export function parseConfigInput(text: string, meta: RequestConfigFieldMeta): ParsedConfigInput {
  const trimmed = text.trim();
  if (!trimmed) return meta.nullable ? { ok: true, value: null } : { ok: false };
  const amount = Number(trimmed.replace(',', '.'));
  if (!Number.isFinite(amount)) return { ok: false };
  const canonical = Math.round(amount * meta.inputScale);
  if (canonical < meta.min || canonical > meta.max) return { ok: false };
  return { ok: true, value: canonical };
}

export function configRangeLabel(meta: RequestConfigFieldMeta, locale: Locale, units: { seconds: string; mebibytes: string }): string {
  const format = (value: number) => numberFormatter(locale).format(value / meta.inputScale);
  const unit = meta.unit === 'milliseconds' ? units.seconds : meta.unit === 'bytes' ? units.mebibytes : '';
  return `${format(meta.min)}–${format(meta.max)}${unit ? ` ${unit}` : ''}`;
}
