import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { ApiError } from '../src/api.js';
import { authorizationCompleteError, authorizationStartError, canReauthorizeAccount, claudeCompletionLimits, claudeCompletionRetryMillis, claudeCompletionStopReason, isAuthorizationIdentityMismatch, parseClaudeCompletion, validAuthorizationCallback } from '../src/operator/authorizationCode.js';
import { authorizationCodeCopy } from '../src/operator/authorizationCodeCopy.js';
import { authorizationJourneyCopy } from '../src/operator/authorizationJourneyCopy.js';

test('generic OAuth help does not assume Google and continuation uses user-facing wording', () => {
  for (const locale of ['zh-CN', 'en']) {
    assert.doesNotMatch(authorizationCodeCopy(locale, 'fixture-plugin').help, /Google/);
    assert.match(authorizationCodeCopy(locale, 'google-antigravity').help, /Google/);
    assert.doesNotMatch(authorizationCodeCopy(locale).recoveryExpires, /已签发|恢复截止|Issued|recovery/);
  }
});

test('complete callback requires an unambiguous full URL, not a code fragment', () => {
  assert.equal(validAuthorizationCallback('http://localhost:8080/callback?code=fixture&state=fixture'), true);
  for (const value of ['fixture#state', '/callback?code=x&state=y', 'javascript:alert(1)', 'https://cb.test/?code=x', 'https://cb.test/?code=x&code=y&state=z', 'https://cb.test/?code=x&state=y&state=z']) {
    assert.equal(validAuthorizationCallback(value), false);
  }
});

test('generic reauthorization needs both authoritative capability and the supported account provider', () => {
  const adapter = { api_version: 'oauth-adapter-v1', flow_kind: 'authorization_code_pkce', login_url: '', poll_url: '', refresh_url: '' } as const;
  const provider = { id: 'google-antigravity', oauth_adapter: adapter };
  assert.equal(canReauthorizeAccount({ driver: provider.id, can_reauthorize: true }, provider), true);
  assert.equal(canReauthorizeAccount({ driver: provider.id, can_reauthorize: false }, provider), false);
  assert.equal(canReauthorizeAccount({ driver: 'other-plugin', can_reauthorize: true }, { ...provider, id: 'other-plugin' }), false);
  assert.equal(canReauthorizeAccount({ driver: 'other-plugin', can_reauthorize: true }, provider), false);
  assert.equal(canReauthorizeAccount({ driver: provider.id, can_reauthorize: true }), false);
  assert.equal(canReauthorizeAccount({ driver: 'native', can_reauthorize: true }, { id: 'native', oauth_adapter: { ...adapter, flow_kind: 'openai_device' } }), true);
});

test('missing deployment client and authority errors get actionable safe copy', () => {
  assert.equal(authorizationStartError(new ApiError('default OAuth client configuration is not provisioned for this provider', 409)), 'admin');
  assert.equal(authorizationStartError(new ApiError('Forbidden', 403)), 'forbidden');
  assert.equal(authorizationStartError(new Error('sensitive-callback-and-token')), 'failed');
  assert.equal(isAuthorizationIdentityMismatch(new ApiError('safe', 409, 'oauth_identity_mismatch')), true);
  assert.equal(isAuthorizationIdentityMismatch(new ApiError('no issued result yet', 409)), false);
  assert.equal(isAuthorizationIdentityMismatch(new ApiError('safe', 502, 'oauth_identity_mismatch')), false);
});

test('Claude completion classifies only known backend error shapes into safe bilingual recovery copy', () => {
  const cases = [
    [400, 'invalid_request', 'invalid request: invalid OAuth session token', 'completeInvalidSession'],
    [400, 'invalid_request', 'invalid request: OAuth login session expired', 'completeExpired'],
    [400, 'invalid_request', 'invalid request: OAuth login session is no longer active', 'completeInactive'],
    [400, 'invalid_request', 'invalid request: Claude OAuth completion must be code#state', 'completeInvalidCode'],
    [400, 'invalid_request', 'invalid request: OAuth state did not match', 'completeStateMismatch'],
    [409, 'conflict', 'conflict: reauthorization must use the same Anthropic account', 'completeIdentityMismatch'],
    [401, 'unauthorized', 'sensitive-code-state-session_token', 'completeUnauthorized'],
    [403, 'forbidden', 'sensitive-code-state-session_token', 'completeForbidden'],
    [502, 'upstream_error', 'sensitive-code-state-session_token', 'completeUnavailable'],
    [503, 'service_overloaded', 'sensitive-code-state-session_token', 'completeOverloaded'],
  ] as const;
  for (const [status, code, message, expected] of cases) {
    assert.equal(authorizationCompleteError(new ApiError(message, status, code)), expected);
    assert.equal(authorizationCompleteError(new ApiError(message, 418, code)), 'completeUncertain');
    assert.equal(authorizationCompleteError(new ApiError(message, status, 'sensitive-code-state-session_token')), 'completeUncertain');
    if (status === 400 || status === 409) {
      assert.equal(authorizationCompleteError(new ApiError(`${message}: sensitive-code-state-session_token`, status, code)), 'completeUncertain');
    }
    for (const locale of ['zh-CN', 'en']) {
      const copy = authorizationJourneyCopy(locale);
      assert.doesNotMatch(copy[expected], /sensitive-code-state-session_token|invalid_request|upstream_error|service_overloaded|Google|本次会话已结束|This session has ended/);
      assert.ok(copy[expected].length > 0);
    }
  }
  for (const reason of [new Error('sensitive-code-state-session_token'), new TypeError('sensitive-code-state-session_token'), new ApiError('invalid request: OAuth state did not match', 400), new ApiError('sensitive-code-state-session_token', 409, 'conflict'), new ApiError('safe', 409, 'oauth_identity_mismatch'), { status: 400, code: 'invalid_request', message: 'invalid request: OAuth state did not match' }]) {
    assert.equal(authorizationCompleteError(reason), 'completeUncertain');
  }
  assert.match(authorizationJourneyCopy('zh-CN').completeUncertain, /关闭表单并检查账号状态.*重新打开表单/);
  assert.match(authorizationJourneyCopy('en').completeUncertain, /Close the form and check the account status.*reopen the form/);
});

test('Claude completion distinguishes pending from a complete account in the current scope', () => {
  const account = { id: 'fixture-account', tenant_id: 'fixture-tenant-id', tenant_external_id: 'fixture-a', name: 'Fixture Claude',
    driver: 'anthropic-claude', auth_kind: 'oauth', connection_method: 'oauth', status: 'active', credential_generation: 2,
    credential_expires_at: null, can_refresh: true, can_rotate: false, can_reauthorize: true, route_count: 0, config: {}, created_at: 1, updated_at: 2 };
  assert.equal(parseClaudeCompletion(account, 'fixture-a', account.id), account);
  assert.deepEqual(parseClaudeCompletion({ status: 'pending', retry_after_seconds: 10 }, 'fixture-a'), { status: 'pending', retry_after_seconds: 10 });
  for (const invalid of [undefined, null, {}, [], 'pending', { id: account.id }, { status: 'unknown' },
    { ...account, status: 'pending' }, { ...account, driver: 'other' }, { ...account, auth_kind: 'api_key' },
    { ...account, connection_method: 'api_key' }, { ...account, credential_generation: 0 }, { ...account, route_count: -1 },
    { ...account, config: [] }, { ...account, name: '' }, { ...account, updated_at: NaN }, { ...account, error: {} },
    { status: 'pending', error: { message: 'synthetic-secret' } }]) {
    assert.throws(() => parseClaudeCompletion(invalid, 'fixture-a'), /^Error: Invalid completion response$/);
  }
  for (const key of Object.keys(account)) {
    const incomplete: Record<string, unknown> = { ...account };
    delete incomplete[key];
    assert.throws(() => parseClaudeCompletion(incomplete, 'fixture-a'), `missing ${key} cannot be treated as success`);
  }
  assert.throws(() => parseClaudeCompletion(account, 'fixture-b'));
  assert.throws(() => parseClaudeCompletion(account, 'fixture-a', 'another-account'));
});

test('Claude pending delays never shorten valid server intervals and invalid intervals have a finite fallback', () => {
  for (const retry_after_seconds of [undefined, null, '0', -1, 0, NaN, Infinity, {}, []]) {
    const result = parseClaudeCompletion({ status: 'pending', retry_after_seconds }, 'fixture-a');
    assert.ok(!('id' in result));
    assert.equal(claudeCompletionRetryMillis(result.retry_after_seconds), 5000);
  }
  for (const seconds of [0.1, 1, 5, 10.5, 300]) {
    assert.ok(claudeCompletionRetryMillis(seconds) >= seconds * 1000);
  }
  assert.equal(claudeCompletionRetryMillis(Number.MAX_VALUE), Number.MAX_SAFE_INTEGER);
  assert.ok(claudeCompletionRetryMillis(Number.MAX_VALUE) > claudeCompletionLimits.durationMillis);
});

test('Claude continuation is bounded by expiry, a local deadline and an attempt cap', () => {
  assert.equal(claudeCompletionStopReason(10_000, 20_000, 1, 9999), undefined);
  assert.equal(claudeCompletionStopReason(10_000, 20_000, 1, 10_000), 'completeExpired');
  assert.equal(claudeCompletionStopReason(30_000, 20_000, 1, 20_000), 'completeLimited');
  assert.equal(claudeCompletionStopReason(undefined, 20_000, claudeCompletionLimits.attempts, 0), 'completeLimited');
  assert.equal(claudeCompletionStopReason(undefined, 20_000, claudeCompletionLimits.attempts - 1, 0), undefined);
  assert.equal(claudeCompletionStopReason(10_000, 10_000, claudeCompletionLimits.attempts, 10_000), 'completeExpired');
  for (const locale of ['zh-CN', 'en']) {
    const copy = authorizationJourneyCopy(locale);
    for (const key of ['completePending', 'completeChecking', 'completeLimited'] as const) {
      assert.ok(copy[key]);
      assert.doesNotMatch(copy[key], /synthetic-secret|session_token|authorization_code/);
    }
  }
});

test('host flow is catalog selected and has no replay, storage, or raw error display', () => {
  const component = readFileSync(new URL('../src/operator/AuthorizationCodeConnection.tsx', import.meta.url), 'utf8');
  const management = readFileSync(new URL('../src/operator/pages/ManagementPages.tsx', import.meta.url), 'utf8');
  assert.match(management, /flow_kind === 'authorization_code_pkce' \? <AuthorizationCodeConnection/);
  assert.match(component, /callback_url: callbackUrl/);
  assert.match(component, /consumed\.current = true/);
  assert.match(component, /setCallback\(''\)/);
  assert.match(component, /widgets=\{fluentFormWidgets\}/);
  assert.doesNotMatch(component, /<input\b|<select\b/);
  assert.match(component, /saved\.current = true/);
  assert.match(component, /try \{ await onChanged\(result\); \}/);
  assert.match(component, /await onChanged\(savedAccount\.current\)/);
  assert.match(component, /copy\.savedButReadFailed/);
  assert.match(component, /setRecoveryDeadline\(result\.recovery_expires_at\)/);
  assert.match(component, /proxy_network_scope: 'private'/);
  assert.match(component, /const callbackUrl = continueIssued \? '' : callback\.trim\(\)/);
  assert.match(component, /Date\.now\(\) >= recoveryDeadline/);
  assert.doesNotMatch(component, /proxy_network_scope: 'public'/);
  assert.doesNotMatch(component, /localStorage|sessionStorage|console\.|apiRead|setInterval|setTimeout|reason\.message/);
  assert.doesNotMatch(component, /google-antigravity|provider-adapter/);
});
