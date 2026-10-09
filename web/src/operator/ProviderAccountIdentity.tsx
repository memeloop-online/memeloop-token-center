import { useI18n } from '../i18n';
import type { UpstreamAccount } from '../types';
import { providerConnectionCopy } from './providerConnectionCopy';

export function ProviderAccountIdentity({ account, editing = false }: { account: UpstreamAccount; editing?: boolean }) {
  const { locale } = useI18n();
  const copy = providerConnectionCopy(locale);
  const expiry = typeof account.credential_expires_at === 'number' ? new Date(account.credential_expires_at) : undefined;
  return <dl className="provider-account-identity">
    {!editing && <div><dt>{copy.displayName}</dt><dd>{account.name}</dd></div>}
    {account.driver === 'kimi-oauth' && <div><dt>{copy.providerIdentity}</dt><dd>{copy.kimiIdentityUnavailable}</dd></div>}
    {account.auth_kind === 'oauth' && <div><dt>{copy.authorizationExpires}</dt><dd>{expiry && Number.isFinite(expiry.getTime())
      ? <time dateTime={expiry.toISOString()}>{expiry.toLocaleString(locale)}</time>
      : copy.authorizationExpiryUnknown}</dd></div>}
  </dl>;
}
