import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { ApiError } from '../src/api.js';
import { authorizationStartError, validAuthorizationCallback } from '../src/operator/authorizationCode.js';

test('complete callback requires an unambiguous full URL, not a code fragment', () => {
  assert.equal(validAuthorizationCallback('http://localhost:8080/callback?code=fixture&state=fixture'), true);
  for (const value of ['fixture#state', '/callback?code=x&state=y', 'javascript:alert(1)', 'https://cb.test/?code=x', 'https://cb.test/?code=x&code=y&state=z', 'https://cb.test/?code=x&state=y&state=z']) {
    assert.equal(validAuthorizationCallback(value), false);
  }
});

test('missing deployment client and authority errors get actionable safe copy', () => {
  assert.equal(authorizationStartError(new ApiError('default OAuth client configuration is not provisioned for this provider', 409)), 'admin');
  assert.equal(authorizationStartError(new ApiError('Forbidden', 403)), 'forbidden');
  assert.equal(authorizationStartError(new Error('sensitive-callback-and-token')), 'failed');
});

test('host flow is catalog selected and has no replay, storage, or raw error display', () => {
  const component = readFileSync(new URL('../src/operator/AuthorizationCodeConnection.tsx', import.meta.url), 'utf8');
  const management = readFileSync(new URL('../src/operator/pages/ManagementPages.tsx', import.meta.url), 'utf8');
  assert.match(management, /flow_kind === 'authorization_code_pkce' \? <AuthorizationCodeConnection/);
  assert.match(component, /callback_url: callbackUrl/);
  assert.match(component, /consumed\.current = true/);
  assert.match(component, /setCallback\(''\)/);
  assert.doesNotMatch(component, /localStorage|sessionStorage|console\.|apiRead|setInterval|setTimeout|reason\.message/);
  assert.doesNotMatch(component, /google-antigravity|upstream_account_id|provider-adapter/);
});
