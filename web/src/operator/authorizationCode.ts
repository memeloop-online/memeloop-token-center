import { ApiError } from '../api.js';
import type { ProviderType, UpstreamAccount } from '../types.js';

export function canReauthorizeAccount(account: Pick<UpstreamAccount, 'driver' | 'can_reauthorize'>, provider?: Pick<ProviderType, 'id' | 'oauth_adapter'>): boolean {
  if (!account.can_reauthorize || !provider || provider.id !== account.driver) return false;
  return provider.oauth_adapter?.flow_kind !== 'authorization_code_pkce' || provider.id === 'google-antigravity';
}

export function isAuthorizationIdentityMismatch(reason: unknown): boolean {
  return reason instanceof ApiError && reason.status === 409 && reason.code === 'oauth_identity_mismatch';
}

export interface AuthorizationCodeSession {
  driver: string;
  login_url: string;
  session_token: string;
  expires_at: number;
  recovery_expires_at: number;
}

export function validAuthorizationCallback(value: string): boolean {
  try {
    const url = new URL(value.trim());
    return ['http:', 'https:'].includes(url.protocol) && !url.username && !url.password
      && url.searchParams.getAll('code').length === 1 && Boolean(url.searchParams.get('code'))
      && url.searchParams.getAll('state').length === 1 && Boolean(url.searchParams.get('state'));
  } catch { return false; }
}

// Never display arbitrary server/network errors: they may contain callback or token data.
export function authorizationStartError(reason: unknown): 'admin' | 'forbidden' | 'failed' {
  if (reason instanceof ApiError && reason.status === 403) return 'forbidden';
  if (reason instanceof ApiError && reason.message.startsWith('default OAuth client configuration')) return 'admin';
  return 'failed';
}
