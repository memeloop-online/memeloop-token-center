import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { clientCredentialPortalUrl, isOrdinaryClientCredential } from '../src/operator/clientCredentialGuidance.js';

const scope = readFileSync(new URL('../src/operator/hooks/useOperatorScope.ts', import.meta.url), 'utf8');
const access = readFileSync(new URL('../src/operator/OperatorAccessSettings.tsx', import.meta.url), 'utf8');
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
  assert.match(scope, /reason instanceof ApiError && reason\.status === 401 && isOrdinaryClientCredential\(candidate\)/);
  assert.match(scope, /messageOf\(reason, t\('common\.connectionFailed'\)\)/);
});

test('a failed credential is never stored and never shown as signed in', () => {
  assert.match(scope, /: \{ credential: '', validated: false, credentialInput: state\.credentialInput, tenant: '', tenants: \[\], status: \{ kind: 'failed', message: action\.message \} \}/);
  assert.match(scope, /clearRememberedCredential\('operator'\)/);
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
  assert.match(i18n, /'operator\.clientKeyRejected': '[^']*mts_[^']*个人中心[^']*'/);
  assert.match(i18n, /'operator\.clientKeyRejected': '[^']*service credential[^']*portal[^']*'/);
  assert.match(i18n, /'operator\.clientCredentialPortalHint': '[^']*mtc_[^']*个人中心[^']*'/);
  assert.match(i18n, /'operator\.clientCredentialPortalHint': '[^']*mtc_[^']*portal[^']*'/);
});
