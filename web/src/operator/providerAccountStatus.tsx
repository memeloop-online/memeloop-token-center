import { useI18n } from '../i18n';
import { useQuotaClock } from './useQuotaClock';
import { providerAccountStatus, type AccountStatusInput } from './providerAccountState';
import { DetailTooltip } from '../design-system';

export { providerAccountStatus } from './providerAccountState';

export function ProviderAccountStatus({ account, credential = false }: { account: AccountStatusInput; credential?: boolean }) {
  const { locale, t } = useI18n();
  const status = providerAccountStatus(account, useQuotaClock());
  const label = t(credential ? status.credentialLabel : status.accountLabel, { time: typeof account.credential_expires_at === 'number' && Number.isFinite(account.credential_expires_at) ? new Date(account.credential_expires_at).toLocaleString(locale) : '' });
  if (!status.renewalLabel && !status.paused) return <span className={`status ${status.tone}`}>{label}</span>;
  const detail = [label, status.renewalLabel ? t(status.renewalLabel) : undefined, status.paused ? t('providers.renewalPaused') : undefined].filter(Boolean).join(' · ');
  return <DetailTooltip content={detail}><span tabIndex={0} className={`status ${status.tone}`}>{status.uncertain && !credential ? t('providers.renewalUnknown') : label}</span></DetailTooltip>;
}
