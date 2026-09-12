import { formatCurrency, formatNumber } from '../format';
import { useI18n } from '../i18n';
import type { KeyView } from '../types';
import { enumLabel } from './scope/operatorShared';

export function CredentialPolicySummary({ policy, currency }: Pick<KeyView, 'policy' | 'currency'>) {
  const { t, locale } = useI18n();
  // EnforcementMode::MeteredUnlimited skips these admission limits. A large
  // numeric limit on a prepaid key is still finite and must never imply infinity.
  const unlimited = policy.enforcement_mode === 'metered_unlimited';
  const rate = (value: number) => unlimited ? t('policy.unlimited') : formatNumber(value, locale);
  const budget = (value: string | null) => unlimited ? t('policy.unlimited') : value === null ? t('policy.notSet') : formatCurrency(value, currency, locale);
  return <div className="policy-chips">
    <span>{enumLabel(t, 'enforcementMode', policy.enforcement_mode)}</span>
    <span>RPM {rate(policy.requests_per_minute)}</span>
    <span>TPM {rate(policy.tokens_per_minute)}</span>
    <span>{t('self.concurrency')} {rate(policy.max_concurrency)}</span>
    <span>{t('budget.daily')}: {budget(policy.daily_budget)}</span>
    <span>{t('budget.weekly')}: {budget(policy.weekly_budget)}</span>
    <span>{t('budget.lifetime')}: {budget(policy.lifetime_budget)}</span>
  </div>;
}
