import type { UpstreamAccount } from '../types.js';

export type AccountStatusInput = Pick<UpstreamAccount, 'status' | 'auth_kind' | 'credential_expires_at'>;

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
