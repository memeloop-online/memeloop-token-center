import assert from 'node:assert/strict';
import test from 'node:test';
import { providerAccountStatus, type AccountStatusInput, type OAuthRefreshStatus } from '../src/operator/providerAccountState.js';

const diagnostic: OAuthRefreshStatus = { credential_generation: 3, access_state: 'expired', refresh_state: 'outcome_unknown', failure_class: 'network', reauthorization_required: false, automatic_refresh_paused: true };
const account: AccountStatusInput = { status: 'active', auth_kind: 'oauth', credential_generation: 3, credential_expires_at: 900, oauth_refresh: diagnostic };

test('0142 network and 7021 local persistence retain distinct uncertainty without inventing revoked authorization', () => {
  for (const [failure_class, label] of [['network', 'providers.renewalNetworkUnknown'], ['local_persistence', 'providers.renewalLocalUnknown'], [null, 'providers.renewalUnknown']] as const) {
    const status = providerAccountStatus({ ...account, oauth_refresh: { ...diagnostic, failure_class } }, 1000);
    assert.equal(status.accountLabel, 'providers.accessTokenExpired');
    assert.equal(status.renewalLabel, label);
    assert.equal(status.paused, true);
    assert.equal(status.tone, 'bad');
  }
});

test('reauthorization is based on current diagnostic evidence and missing diagnostics stay uncertain', () => {
  for (const oauth_refresh of [
    { ...diagnostic, access_state: 'revoked' as const },
    { ...diagnostic, reauthorization_required: true },
    { ...diagnostic, refresh_state: 'reauthorization_required' as const },
  ]) assert.equal(providerAccountStatus({ ...account, oauth_refresh }, 1000).accountLabel, 'providers.authorizationRevoked');
  assert.equal(providerAccountStatus({ ...account, oauth_refresh: undefined }, 1000).accountLabel, 'providers.accessTokenExpired');
  assert.equal(providerAccountStatus({ ...account, credential_generation: 4, oauth_refresh: { ...diagnostic, reauthorization_required: true } }, 1000).renewalLabel, undefined);
  assert.equal(providerAccountStatus({ ...account, credential_generation: 4, credential_expires_at: 2000 }, 1000).accountLabel, 'providers.accountEnabled');
  assert.equal(providerAccountStatus({ ...account, status: 'disabled' }, 1000).accountLabel, 'status.disabled');
  assert.equal(providerAccountStatus({ ...account, oauth_refresh: undefined, credential_expires_at: null }, 1000).tone, 'pending');
});

test('local pending, active renewal and safe unsent failure remain different', () => {
  for (const [refresh_state, label] of [['pending_local', 'providers.renewalPendingLocal'], ['in_progress', 'providers.renewalInProgress'], ['failed', 'providers.renewalFailed']] as const) {
    const status = providerAccountStatus({ ...account, credential_expires_at: 2000, oauth_refresh: { ...diagnostic, access_state: 'valid', refresh_state, automatic_refresh_paused: false } }, 1000);
    assert.equal(status.renewalLabel, label);
    assert.equal(status.tone, 'pending');
    assert.equal(status.paused, false);
  }
});
