import { useI18n } from './i18n';
import type { BudgetLimitSnapshot, KeyLimitSnapshot } from './types';

function BudgetState({ label, value, currency, locale }: { label: string; value: BudgetLimitSnapshot; currency: string; locale: string }) {
  const { t } = useI18n();
  const reset = value.reset_at === null ? '' : ` · ${t('limits.reset')} ${new Date(value.reset_at).toLocaleString(locale)}`;
  return <span><b>{label}</b>{value.settled} + {value.reserved} / {value.limit ?? '∞'} {currency} · {t('limits.remaining')} {value.remaining ?? '∞'}{reset}</span>;
}

export function LimitSnapshot({ value }: { value: KeyLimitSnapshot }) {
  const { locale, t } = useI18n();
  const rate = (name: string, limit: KeyLimitSnapshot['rpm']) => <span><b>{name}</b>{limit.used.toLocaleString(locale)} / {limit.limit.toLocaleString(locale)} · {t('limits.remaining')} {limit.remaining.toLocaleString(locale)} · {t('limits.reset')} {new Date(limit.reset_at).toLocaleString(locale)}</span>;
  return <div className="inline-editor"><h3>{t('limits.snapshot')}</h3><div className="policy-grid">
    <span><b>{t('self.balance', { currency: value.currency })}</b>{value.available_balance}</span>
    <span><b>{t('limits.reservedBalance')}</b>{value.reserved_balance} {value.currency}</span>
    {rate('RPM', value.rpm)}
    {rate('TPM', value.tpm)}
    <span><b>{t('self.concurrency')}</b>{value.concurrency.active.toLocaleString(locale)} / {value.concurrency.limit.toLocaleString(locale)} · {t('limits.remaining')} {value.concurrency.remaining.toLocaleString(locale)}</span>
    <BudgetState label={t('budget.daily')} value={value.daily_budget} currency={value.currency} locale={locale} />
    <BudgetState label={t('budget.weekly')} value={value.weekly_budget} currency={value.currency} locale={locale} />
    <BudgetState label={t('budget.lifetime')} value={value.lifetime_budget} currency={value.currency} locale={locale} />
  </div></div>;
}
