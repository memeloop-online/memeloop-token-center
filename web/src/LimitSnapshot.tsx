import { formatCompactCurrency, formatMetricDisplay } from './format';
import { useI18n } from './i18n';
import type { BudgetLimitSnapshot, KeyLimitSnapshot, KeyView } from './types';
import './LimitSnapshot.css';

function Amount({ value, currency }: { value: string; currency: string }) {
  const { locale } = useI18n();
  const formatted = formatCompactCurrency(value, currency, locale);
  return <span title={formatted.title}>{formatted.text}</span>;
}

function Count({ value }: { value: number }) {
  const { locale } = useI18n();
  const formatted = formatMetricDisplay(value, locale);
  return <span title={formatted.title}>{formatted.text}</span>;
}

function BudgetState({ label, value, currency }: { label: string; value: BudgetLimitSnapshot; currency: string }) {
  const { locale, t } = useI18n();
  return <div className="limit-cell"><b>{label}</b><div><Amount value={value.settled} currency={currency} /> + <Amount value={value.reserved} currency={currency} /> / {value.limit === null ? t('policy.unlimited') : <Amount value={value.limit} currency={currency} />}</div>
    <small>{t('limits.remaining')} {value.remaining === null ? t('policy.unlimited') : <Amount value={value.remaining} currency={currency} />}</small>
    {value.reset_at !== null && <small>{t('limits.reset')} {new Date(value.reset_at).toLocaleString(locale)}</small>}
  </div>;
}

export function LimitSnapshot({ value, enforcementMode }: { value: KeyLimitSnapshot; enforcementMode?: KeyView['policy']['enforcement_mode'] }) {
  const { locale, t } = useI18n();
  const rate = (name: string, limit: KeyLimitSnapshot['rpm']) => <div className="limit-cell"><b>{name}</b><div><Count value={limit.used} /> / <Count value={limit.limit} /></div><small>{t('limits.remaining')} <Count value={limit.remaining} /></small><small>{t('limits.reset')} {new Date(limit.reset_at).toLocaleString(locale)}</small></div>;
  return <div className="limit-snapshot"><h3>{t('limits.snapshot')}</h3>
    {enforcementMode === 'metered_unlimited' ? <p>{t('schema.Prepaid enforces balance and policy limits synchronously. Metered unlimited records exact postpaid usage without those shared admission limits.')}</p> : <div className="limit-grid">
      <div className="limit-cell"><b>{t('self.balance', { currency: value.currency })}</b><Amount value={value.available_balance} currency={value.currency} /></div>
      <div className="limit-cell"><b>{t('limits.reservedBalance')}</b><Amount value={value.reserved_balance} currency={value.currency} /></div>
      {rate('RPM', value.rpm)}
      {rate('TPM', value.tpm)}
      <div className="limit-cell"><b>{t('self.concurrency')}</b><div><Count value={value.concurrency.active} /> / <Count value={value.concurrency.limit} /></div><small>{t('limits.remaining')} <Count value={value.concurrency.remaining} /></small></div>
      <BudgetState label={t('budget.daily')} value={value.daily_budget} currency={value.currency} />
      <BudgetState label={t('budget.weekly')} value={value.weekly_budget} currency={value.currency} />
      <BudgetState label={t('budget.lifetime')} value={value.lifetime_budget} currency={value.currency} />
    </div>}
  </div>;
}
