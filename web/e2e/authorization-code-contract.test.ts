import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { ApiError } from '../src/api.js';
import { authorizationStartError, canReauthorizeAccount, isAuthorizationIdentityMismatch, validAuthorizationCallback } from '../src/operator/authorizationCode.js';
import { authorizationCodeCopy } from '../src/operator/authorizationCodeCopy.js';

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
  assert.match(component, /try \{ await onChanged\(\); \}/);
  assert.match(component, /copy\.savedButReadFailed/);
  assert.match(component, /setRecoveryDeadline\(result\.recovery_expires_at\)/);
  assert.match(component, /proxy_network_scope: 'private'/);
  assert.match(component, /const callbackUrl = continueIssued \? '' : callback\.trim\(\)/);
  assert.match(component, /Date\.now\(\) >= recoveryDeadline/);
  assert.doesNotMatch(component, /proxy_network_scope: 'public'/);
  assert.doesNotMatch(component, /localStorage|sessionStorage|console\.|apiRead|setInterval|setTimeout|reason\.message/);
  assert.doesNotMatch(component, /google-antigravity|provider-adapter/);
});
