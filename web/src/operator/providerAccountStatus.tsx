import { useI18n } from '../i18n';
import { useQuotaClock } from './useQuotaClock';
import { providerAccountStatus, type AccountStatusInput } from './providerAccountState';

export { providerAccountStatus } from './providerAccountState';

export function ProviderAccountStatus({ account, credential = false }: { account: AccountStatusInput; credential?: boolean }) {
  const { locale, t } = useI18n();
  const status = providerAccountStatus(account, useQuotaClock());
  return <span className={`status ${status.tone}`}>{t(credential ? status.credentialLabel : status.accountLabel, { time: typeof account.credential_expires_at === 'number' && Number.isFinite(account.credential_expires_at) ? new Date(account.credential_expires_at).toLocaleString(locale) : '' })}</span>;
}
