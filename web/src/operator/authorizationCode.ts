import { ApiError } from '../api.js';
import type { ProviderType, UpstreamAccount } from '../types.js';

export function canReauthorizeAccount(account: Pick<UpstreamAccount, 'driver' | 'can_reauthorize'>, provider?: Pick<ProviderType, 'id' | 'oauth_adapter'>): boolean {
  if (!account.can_reauthorize || !provider || provider.id !== account.driver) return false;
  return provider.oauth_adapter?.flow_kind !== 'authorization_code_pkce' || provider.id === 'google-antigravity';
}

export function isAuthorizationIdentityMismatch(reason: unknown): boolean {
  return reason instanceof ApiError && reason.status === 409 && reason.code === 'oauth_identity_mismatch';
}

export function authorizationCompleteError(reason: unknown) {
  if (!(reason instanceof ApiError)) return 'completeUncertain';
  if (reason.status === 400 && reason.code === 'invalid_request') {
    switch (reason.message) {
      case 'invalid request: invalid OAuth session token': return 'completeInvalidSession';
      case 'invalid request: OAuth login session expired': return 'completeExpired';
      case 'invalid request: OAuth login session is no longer active': return 'completeInactive';
      case 'invalid request: Claude OAuth completion must be code#state': return 'completeInvalidCode';
      case 'invalid request: OAuth state did not match': return 'completeStateMismatch';
    }
  }
  if (reason.status === 409 && reason.code === 'conflict'
    && reason.message === 'conflict: reauthorization must use the same Anthropic account') return 'completeIdentityMismatch';
  if (reason.status === 401 && reason.code === 'unauthorized') return 'completeUnauthorized';
  if (reason.status === 403 && reason.code === 'forbidden') return 'completeForbidden';
  if (reason.status === 502 && reason.code === 'upstream_error') return 'completeUnavailable';
  if (reason.status === 503 && reason.code === 'service_overloaded') return 'completeOverloaded';
  return 'completeUncertain';
}

export const claudeCompletionLimits = { attempts: 20, durationMillis: 120_000, requestMillis: 30_000 } as const;

export type ClaudeCompletion = { status: 'pending'; retry_after_seconds: number } | UpstreamAccount;

export function parseClaudeCompletion(value: unknown, tenant: string, accountId?: string): ClaudeCompletion {
  if (!value || typeof value !== 'object' || Array.isArray(value) || 'error' in value) throw new Error('Invalid completion response');
  const result = value as Record<string, unknown>;
  if (result.status === 'pending' && !('id' in result)) {
    return { status: 'pending', retry_after_seconds: typeof result.retry_after_seconds === 'number'
      && Number.isFinite(result.retry_after_seconds) && result.retry_after_seconds > 0 ? result.retry_after_seconds : 5 };
  }
  if (['id', 'tenant_id', 'name', 'status'].some(key => typeof result[key] !== 'string' || !(result[key] as string).trim())
    || result.status === 'pending' || result.driver !== 'anthropic-claude' || result.auth_kind !== 'oauth' || result.connection_method !== 'oauth'
    || result.tenant_external_id !== tenant || (accountId !== undefined && result.id !== accountId)
    || !Number.isSafeInteger(result.credential_generation) || (result.credential_generation as number) < 1
    || !Number.isSafeInteger(result.route_count) || (result.route_count as number) < 0
    || ['created_at', 'updated_at'].some(key => typeof result[key] !== 'number' || !Number.isFinite(result[key]) || (result[key] as number) < 0)
    || (result.credential_expires_at !== null && (typeof result.credential_expires_at !== 'number' || !Number.isFinite(result.credential_expires_at)))
    || ['can_refresh', 'can_rotate', 'can_reauthorize'].some(key => typeof result[key] !== 'boolean')
    || !result.config || typeof result.config !== 'object' || Array.isArray(result.config)) throw new Error('Invalid completion response');
  return result as unknown as UpstreamAccount;
}

export function claudeCompletionRetryMillis(seconds: number): number {
  return Math.max(1000, Math.min(Number.MAX_SAFE_INTEGER, Math.ceil(seconds * 1000)));
}

export function claudeCompletionStopReason(expiresAt: number | undefined, deadline: number, attempts: number, now: number) {
  if (expiresAt !== undefined && Number.isFinite(expiresAt) && now >= expiresAt) return 'completeExpired';
  if (now >= deadline || attempts >= claudeCompletionLimits.attempts) return 'completeLimited';
  return undefined;
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
