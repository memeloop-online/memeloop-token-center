import type { UpstreamAccount } from '../types.js';

export interface OAuthRefreshStatus {
  credential_generation: number;
  access_state: 'valid' | 'expired' | 'revoked' | 'unknown';
  refresh_state: 'not_observed' | 'in_progress' | 'pending_local' | 'outcome_unknown' | 'failed' | 'reauthorization_required';
  failure_class: string | null;
  reauthorization_required: boolean;
  automatic_refresh_paused: boolean;
  allowed_actions?: string[];
  lease_expires_at?: number | null;
}

export type AccountStatusInput = Pick<UpstreamAccount, 'status' | 'auth_kind' | 'credential_expires_at'> & {
  credential_generation?: number;
  oauth_refresh?: OAuthRefreshStatus | null;
};

export function providerAccountStatus(account: AccountStatusInput, now: number) {
  const expiry = account.credential_expires_at;
  const known = typeof expiry === 'number' && Number.isFinite(expiry);
  const diagnostic = account.auth_kind === 'oauth' && account.oauth_refresh?.credential_generation === account.credential_generation ? account.oauth_refresh : undefined;
  const expired = diagnostic?.access_state === 'expired' || (diagnostic?.access_state !== 'valid' && known && expiry <= now);
  const revoked = diagnostic?.access_state === 'revoked';
  const renewalLabel = diagnostic?.refresh_state === 'outcome_unknown'
    ? diagnostic.failure_class === 'network' ? 'providers.renewalNetworkUnknown' : diagnostic.failure_class === 'local_persistence' ? 'providers.renewalLocalUnknown' : 'providers.renewalUnknown'
    : diagnostic?.refresh_state === 'pending_local' ? 'providers.renewalPendingLocal'
      : diagnostic?.refresh_state === 'in_progress' ? 'providers.renewalInProgress'
        : diagnostic?.refresh_state === 'failed' ? 'providers.renewalFailed' : undefined;
  const credentialLabel = revoked ? 'providers.authorizationRevoked' : expired ? account.auth_kind === 'oauth' ? 'providers.accessTokenExpired' : 'providers.credentialExpired' : known ? 'providers.credentialExpires' : 'providers.credentialExpiryUnknown';
  return {
    expired,
    uncertain: account.status === 'active' && !revoked && diagnostic?.refresh_state === 'outcome_unknown',
    needsReauthorization: diagnostic?.reauthorization_required === true,
    allowedActions: diagnostic?.allowed_actions,
    credentialLabel,
    renewalLabel,
    paused: diagnostic?.automatic_refresh_paused === true,
    accountLabel: account.status !== 'active' ? 'status.disabled' : revoked || expired ? credentialLabel : !known && account.auth_kind === 'oauth' ? 'providers.accountEnabledUnknown' : 'providers.accountEnabled',
    tone: revoked || expired ? 'bad' : renewalLabel || account.status !== 'active' || !known ? 'pending' : 'ok',
  };
}
