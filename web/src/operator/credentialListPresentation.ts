import { formatCurrency } from '../format.js';
import type { Locale } from '../i18n.js';
import type { KeyView } from '../types.js';

type Translate = (key: string, variables?: Record<string, string | number>) => string;

function compactCurrency(value: string, currency: string, locale: Locale) {
  const numeric = Number(value);
  if (!Number.isFinite(numeric) || Math.abs(numeric) < 10_000) return formatCurrency(value, currency, locale);
  try {
    return new Intl.NumberFormat(locale, { style: 'currency', currency, currencyDisplay: 'symbol', notation: 'compact', maximumFractionDigits: 2 }).format(numeric);
  } catch {
    return formatCurrency(value, currency, locale);
  }
}

/** Display-only summary. Enforcement mode, never a large balance, determines
 * whether a credential is unlimited. */
export function credentialBudgetPresentation(value: KeyView, locale: Locale, t: Translate) {
  if (value.policy.enforcement_mode === 'metered_unlimited') {
    return { text: t('credentials.meteredUnlimited'), title: t('credentials.meteredUnlimitedHint') };
  }
  const exact = formatCurrency(value.available_balance, value.currency, locale);
  return { text: t('credentials.availableBalance', { amount: compactCurrency(value.available_balance, value.currency, locale) }), title: t('credentials.availableBalanceDetail', { amount: exact }) };
}
