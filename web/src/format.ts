import type { Locale } from './i18n.js';

export interface FormattedValue {
  text: string;
  title?: string;
  compact?: string;
}

const numberLocale = (locale: Locale) => locale === 'en' ? 'en-US' : 'zh-CN';

interface FixedDecimal {
  fraction: string;
  integer: string;
  negative: boolean;
}

const numericPartTypes = new Set<Intl.NumberFormatPartTypes>(['integer', 'group', 'decimal', 'fraction']);

function parseFixedDecimal(value: string | number): FixedDecimal | undefined {
  const raw = String(value).trim();
  const match = /^([+-]?)(?:(\d+)(?:\.(\d*))?|\.(\d+))(?:[eE]([+-]?\d+))?$/.exec(raw);
  if (!match) return undefined;
  const integerDigits = match[2] ?? '0';
  const fractionDigits = match[3] ?? match[4] ?? '';
  const exponent = Number(match[5] ?? 0);
  if (!Number.isSafeInteger(exponent)) return undefined;
  const decimalPoint = integerDigits.length + exponent;
  const digits = integerDigits + fractionDigits;
  // Avoid letting an untrusted exponent allocate an unbounded display string.
  if (decimalPoint < -10_000 || decimalPoint > digits.length + 10_000) return undefined;
  const expandedInteger = decimalPoint <= 0
    ? '0'
    : decimalPoint >= digits.length
      ? digits + '0'.repeat(decimalPoint - digits.length)
      : digits.slice(0, decimalPoint);
  const expandedFraction = decimalPoint <= 0
    ? '0'.repeat(-decimalPoint) + digits
    : decimalPoint >= digits.length
      ? ''
      : digits.slice(decimalPoint);
  return {
    fraction: expandedFraction,
    integer: expandedInteger.replace(/^0+(?=\d)/, ''),
    negative: match[1] === '-',
  };
}

function localizedFixedDecimal(value: FixedDecimal, locale: Locale) {
  const parts = new Intl.NumberFormat(numberLocale(locale), { useGrouping: true }).formatToParts(12_345.6);
  const group = parts.find((part) => part.type === 'group')?.value ?? ',';
  const decimal = parts.find((part) => part.type === 'decimal')?.value ?? '.';
  const groups: string[] = [];
  for (let end = value.integer.length; end > 0; end -= 3) groups.unshift(value.integer.slice(Math.max(0, end - 3), end));
  return `${groups.join(group)}${value.fraction ? `${decimal}${value.fraction}` : ''}`;
}

export function formatNumber(value: number, locale: Locale, maximumFractionDigits = 0) {
  if (!Number.isFinite(value)) return '—';
  return new Intl.NumberFormat(numberLocale(locale), { maximumFractionDigits }).format(value);
}

export function formatMetricNumber(value: number | null | undefined, locale: Locale): FormattedValue {
  if (value === null || value === undefined || !Number.isFinite(value)) return { text: '—' };
  const exact = formatNumber(value, locale);
  const absolute = Math.abs(value);
  if (absolute < 10_000) return { text: exact };
  const compact = new Intl.NumberFormat(numberLocale(locale), {
    notation: 'compact',
    compactDisplay: 'short',
    maximumFractionDigits: 2,
  }).format(value);
  return compact === exact ? { text: exact } : { text: exact, compact };
}

export function formatDecimal(value: string | number | null | undefined, locale: Locale, maximumFractionDigits = 6) {
  if (value === null || value === undefined || value === '') return '—';
  const numeric = typeof value === 'number' ? value : Number(value);
  if (!Number.isFinite(numeric)) return String(value);
  return new Intl.NumberFormat(numberLocale(locale), {
    maximumFractionDigits,
    minimumFractionDigits: 0,
  }).format(numeric);
}

export function formatCurrency(value: string | number | null | undefined, currency: string, locale: Locale) {
  if (value === null || value === undefined || value === '') return '—';
  if (typeof value === 'number' && !Number.isFinite(value)) return `${value} ${currency}`.trim();
  const fixed = parseFixedDecimal(value);
  if (!fixed) return `${value} ${currency}`.trim();
  const exact = localizedFixedDecimal(fixed, locale);
  try {
    const parts = new Intl.NumberFormat(numberLocale(locale), {
      style: 'currency',
      currency,
      currencyDisplay: 'symbol',
      maximumFractionDigits: 0,
      minimumFractionDigits: 0,
      useGrouping: false,
    }).formatToParts(fixed.negative ? -1 : 1);
    let inserted = false;
    return parts.map((part) => {
      if (!numericPartTypes.has(part.type)) return part.value;
      if (inserted) return '';
      inserted = true;
      return exact;
    }).join('');
  } catch {
    return `${fixed.negative ? '-' : ''}${exact} ${currency}`.trim();
  }
}

export function formatPercent(value: number | null | undefined, locale: Locale) {
  if (value === null || value === undefined || !Number.isFinite(value)) return '—';
  const options: Intl.NumberFormatOptions = {
    style: 'percent',
    maximumFractionDigits: 2,
  };
  let formatter = new Intl.NumberFormat(numberLocale(locale), options);
  if (value >= 0 && value < 1 && formatter.formatToParts(value).filter((part) => part.type === 'integer').map((part) => part.value).join('') === '100') {
    for (let digits = 3; digits <= 20; digits += 1) {
      const candidate = new Intl.NumberFormat(numberLocale(locale), { ...options, maximumFractionDigits: digits });
      const integer = candidate.formatToParts(value).filter((part) => part.type === 'integer').map((part) => part.value).join('');
      if (integer !== '100') {
        formatter = candidate;
        break;
      }
    }
  }
  return formatter.format(value);
}

export function formatMilliseconds(value: number | null | undefined, locale: Locale) {
  if (value === null || value === undefined || !Number.isFinite(value)) return '—';
  return `${formatNumber(value, locale, 2)} ms`;
}
