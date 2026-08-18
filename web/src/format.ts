import type { Locale } from './i18n';

export interface FormattedValue {
  text: string;
  title?: string;
}

const numberLocale = (locale: Locale) => locale === 'en' ? 'en-US' : 'zh-CN';

export function formatNumber(value: number, locale: Locale, maximumFractionDigits = 0) {
  if (!Number.isFinite(value)) return '—';
  return new Intl.NumberFormat(numberLocale(locale), { maximumFractionDigits }).format(value);
}

export function formatMetricNumber(value: number | null | undefined, locale: Locale): FormattedValue {
  if (value === null || value === undefined || !Number.isFinite(value)) return { text: '—' };
  const exact = formatNumber(value, locale);
  if (locale !== 'zh-CN') return { text: exact };
  const absolute = Math.abs(value);
  const unit = absolute >= 100_000_000 ? { divisor: 100_000_000, suffix: '亿' }
    : absolute >= 10_000 ? { divisor: 10_000, suffix: '万' }
      : undefined;
  if (!unit) return { text: exact };
  const compact = new Intl.NumberFormat('zh-CN', { maximumFractionDigits: 2 }).format(value / unit.divisor);
  return { text: `${compact}${unit.suffix}`, title: exact };
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
  const numeric = typeof value === 'number' ? value : Number(value);
  if (!Number.isFinite(numeric)) return `${value} ${currency}`.trim();
  try {
    return new Intl.NumberFormat(numberLocale(locale), {
      style: 'currency',
      currency,
      currencyDisplay: 'symbol',
      maximumFractionDigits: 6,
      minimumFractionDigits: 0,
    }).format(numeric);
  } catch {
    return `${formatDecimal(numeric, locale)} ${currency}`.trim();
  }
}

export function formatPercent(value: number | null | undefined, locale: Locale) {
  if (value === null || value === undefined || !Number.isFinite(value)) return '—';
  return new Intl.NumberFormat(numberLocale(locale), {
    style: 'percent',
    maximumFractionDigits: 2,
  }).format(value);
}

export function formatMilliseconds(value: number | null | undefined, locale: Locale) {
  if (value === null || value === undefined || !Number.isFinite(value)) return '—';
  return `${formatNumber(value, locale, 2)} ms`;
}
