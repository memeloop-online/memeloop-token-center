import { useI18n } from '../i18n';
import type { UpstreamAccount } from '../types';
import { useQuotaClock } from './useQuotaClock';

type AccountStatusInput = Pick<UpstreamAccount, 'status' | 'auth_kind' | 'credential_expires_at'>;

export function providerAccountStatus(account: AccountStatusInput, now: number) {
  const expiry = account.credential_expires_at;
  const known = typeof expiry === 'number' && Number.isFinite(expiry);
  const expired = known && expiry <= now;
  return {
    expired,
    credentialLabel: expired ? account.auth_kind === 'oauth' ? 'providers.authorizationExpired' : 'providers.credentialExpired' : known ? 'providers.credentialExpires' : 'providers.credentialExpiryUnknown',
    accountLabel: account.status !== 'active' ? 'status.disabled' : expired ? account.auth_kind === 'oauth' ? 'providers.authorizationExpired' : 'providers.credentialExpired' : !known && account.auth_kind === 'oauth' ? 'providers.accountEnabledUnknown' : 'providers.accountEnabled',
    tone: expired ? 'bad' : account.status === 'active' && known ? 'ok' : 'pending',
  };
}

export function ProviderAccountStatus({ account, credential = false }: { account: AccountStatusInput; credential?: boolean }) {
  const { locale, t } = useI18n();
  const status = providerAccountStatus(account, useQuotaClock());
  return <span className={`status ${status.tone}`}>{t(credential ? status.credentialLabel : status.accountLabel, { time: typeof account.credential_expires_at === 'number' && Number.isFinite(account.credential_expires_at) ? new Date(account.credential_expires_at).toLocaleString(locale) : '' })}</span>;
}
