import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { clientCredentialPortalUrl, isOrdinaryClientCredential } from '../src/operator/clientCredentialGuidance.js';

const scope = readFileSync(new URL('../src/operator/hooks/useOperatorScope.ts', import.meta.url), 'utf8');
const access = readFileSync(new URL('../src/operator/OperatorAccessSettings.tsx', import.meta.url), 'utf8');
const operator = readFileSync(new URL('../src/operator/Operator.tsx', import.meta.url), 'utf8');
const i18n = readFileSync(new URL('../src/i18n.tsx', import.meta.url), 'utf8');

const keyId = 'a1b2c3d4e5f60718293a4b5c6d7e8f90';
const material = 'Aa0-_'.repeat(9);

test('only the issued client-key shape is redirected to the portal guidance', () => {
  assert.equal(isOrdinaryClientCredential(`mtc_${keyId}_${material}`), true);
  assert.equal(isOrdinaryClientCredential(`  mtc_${keyId}_${material}  `), true);
  assert.equal(isOrdinaryClientCredential(`mtc_${keyId.toUpperCase()}_${material}`), true);
  assert.equal(isOrdinaryClientCredential(`mts_${keyId}_${material}`), false);
  assert.equal(isOrdinaryClientCredential('isolated-custom-bootstrap-token-at-least-32-bytes'), false);
  assert.equal(isOrdinaryClientCredential(`mtc_${keyId}_short`), false);
  assert.equal(isOrdinaryClientCredential('mtc_not-a-client-key'), false);
  assert.equal(isOrdinaryClientCredential(''), false);
});

test('every candidate attempts tenant discovery and success still remembers the credential', () => {
  const requestAt = scope.indexOf("api<TenantView[]>('/internal/v1/tenants'");
  const rememberAt = scope.indexOf("rememberCredential('operator', candidate)");
  const guidanceAt = scope.indexOf("t('operator.clientKeyRejected')");
  assert.ok(requestAt > -1 && rememberAt > -1 && guidanceAt > -1);
  assert.ok(requestAt < guidanceAt, 'guidance may only follow the server response');
  assert.ok(requestAt < rememberAt, 'a successful discovery still remembers the credential');
  assert.equal(scope.indexOf('isOrdinaryClientCredential(candidate)') > requestAt, true);
});

test('only a server 401 against the client-key shape shows the portal guidance', () => {
  const catchAt = scope.indexOf('} catch (reason)');
  const guidanceAt = scope.indexOf("t('operator.clientKeyRejected')");
  assert.ok(catchAt > -1 && guidanceAt > catchAt);
  assert.match(scope, /reason instanceof ApiError && reason\.status === 401/);
  assert.match(scope, /isOrdinaryClientCredential\(candidate\)\s*\?\s*t\('operator\.clientKeyRejected'\)/);
  assert.match(scope, /messageOf\(reason, t\('common\.connectionFailed'\)\)/);
});

test('a rejected credential is never stored and never shown as signed in', () => {
  assert.match(scope, /return \{ credential: '', validated: false, credentialInput: state\.credentialInput, tenant: '', tenants: \[\], status: \{ kind: 'failed', message: action\.message \} \}/);
  assert.match(scope, /dispatch\(\{ type: 'authentication-rejected', message \}\)/);
});

test('a 403 permission denial is a rejection with the server message, never a 503-style outage', () => {
  assert.match(scope, /reason instanceof ApiError && reason\.status === 403/);
  assert.match(scope, /authentication-rejected', message: messageOf\(reason, t\('common\.connectionFailed'\)\)/);
  assert.match(scope, /reason\.status === 403\) \{\s*clearRememberedCredential\('operator'\);\s*dispatch\(\{ type: 'authentication-rejected'/);
});

test('only a definitive 401 or 403, or an explicit user clear, removes the remembered credential', () => {
  const clearSites = [...scope.matchAll(/clearRememberedCredential\('operator'\)/g)].map((match) => match.index ?? 0);
  assert.equal(clearSites.length, 4, '401 and 403 rejections in authenticate and refresh, plus the explicit clear control');
  for (const at of clearSites) {
    const context = scope.slice(Math.max(0, at - 320), at);
    assert.ok(/reason\.status === 401|reason\.status === 403|function clearCredential/.test(context), 'every clearing site follows a definitive rejection or the explicit clear control');
  }
});

test('a network, 502, 503 or database outage preserves the session, page and draft', () => {
  assert.match(scope, /\{ \.\.\.state, status: \{ kind: 'unavailable', message: action\.message, candidate: action\.candidate \} \}/);
  assert.match(scope, /dispatch\(\{ type: 'authentication-unavailable', message: outageMessage\(reason\), candidate \}\)/);
  assert.match(scope, /state\.validated && \(state\.status\.kind === 'ready' \|\| state\.status\.kind === 'unavailable'\)/);
  assert.match(scope, /t\('operator\.serviceUnavailable'\)/);
  assert.match(scope, /apiDiagnosticMessage\(reason, t\('common\.connectionFailed'\)/);
});

test('a failed tenant refresh is classified the same way instead of surfacing as a logout', () => {
  const refreshAt = scope.indexOf('async function refreshTenants()');
  assert.ok(refreshAt > -1);
  const refresh = scope.slice(refreshAt);
  assert.match(refresh, /\} catch \(reason\) \{/);
  assert.match(refresh, /reason instanceof ApiError && reason\.status === 401/);
  assert.match(refresh, /dispatch\(\{ type: 'authentication-unavailable', message: outageMessage\(reason\), candidate: credential \}\)/);
});

test('retry re-attempts the preserved candidate without new side effects', () => {
  assert.match(scope, /if \(current\.status\.kind !== 'unavailable'\) return;/);
  assert.match(scope, /void authenticate\(current\.status\.candidate\)/);
  assert.match(operator, /scope\.serviceFault && <div className="notice error" role="alert">\{scope\.serviceFault\}<Button appearance="secondary" onClick=\{scope\.retry\} disabled=\{scope\.authenticating\}>/);
});

test('the access card reuses Fluent controls and drops scoped native buttons', () => {
  assert.match(access, /<SecretInput fluent id="operator-access-credential"/);
  assert.match(access, /<Button appearance="primary" type="submit"/);
  assert.match(access, /<Button appearance="secondary" type="button" onClick=\{onClear\}>/);
  assert.match(access, /<CopyButton fluent value=\{credentialInput\}/);
  assert.doesNotMatch(access, /<button/);
});

test('portal guidance is a bare link that never carries the credential', () => {
  const portal = clientCredentialPortalUrl('https://token-operator.example.test');
  assert.equal(portal, 'https://token.example.test/portal');
  assert.equal(clientCredentialPortalUrl('https://mtc.example.test'), 'https://mtc.example.test/portal');
  assert.equal(clientCredentialPortalUrl('http://localhost:8080'), 'http://localhost:8080/portal');
  assert.equal(clientCredentialPortalUrl('https://token-operator.example.test/operator?view=requests#fragment'), portal);
  assert.doesNotMatch(portal, /[?#]/);
  assert.match(access, /<a href=\{clientCredentialPortalUrl\(window\.location\.origin\)\} target="_blank" rel="noopener noreferrer">/);
  assert.match(access, /t\('operator\.openPortal'\)/);
  assert.match(access, /t\('operator\.clientCredentialPortalHint'\)/);
  assert.doesNotMatch(access, /clientCredentialPortalUrl\s*\+|\$\{[^}]*credential/i);
  assert.doesNotMatch(access, /URLSearchParams|location\.(?:href|search|hash)|history\./);
});

test('rejection and portal-guidance copy exists in both locales', () => {
  assert.equal(i18n.match(/'operator\.clientKeyRejected': '/g)?.length, 2);
  assert.equal(i18n.match(/'operator\.clientCredentialPortalHint': '/g)?.length, 2);
  assert.equal(i18n.match(/'operator\.serviceUnavailable': '/g)?.length, 2);
  assert.match(i18n, /'operator\.clientKeyRejected': '[^']*mts_[^']*个人中心[^']*'/);
  assert.match(i18n, /'operator\.clientKeyRejected': '[^']*service credential[^']*portal[^']*'/);
  assert.match(i18n, /'operator\.clientCredentialPortalHint': '[^']*mtc_[^']*个人中心[^']*'/);
  assert.match(i18n, /'operator\.clientCredentialPortalHint': '[^']*mtc_[^']*portal[^']*'/);
});
