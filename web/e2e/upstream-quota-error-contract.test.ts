import assert from 'node:assert/strict';
import test from 'node:test';
import { quotaReadErrorMessage } from '../src/operator/upstreamQuota.js';

test('normalized quota read failures have distinct safe recovery semantics', () => {
  const expected = {
    credential_invalid: 'quota.errorCredential',
    quota_not_authorized: 'quota.errorSupplierAuthorization',
    quota_destination_invalid: 'quota.errorDestination',
    quota_transport_failed: 'quota.errorTransport',
    quota_timeout: 'quota.errorTimeout',
    quota_rate_limited: 'quota.errorRateLimited',
    quota_busy: 'quota.errorBusy',
    quota_refresh_in_progress: 'quota.errorBusy',
    quota_response_too_large: 'quota.errorPayload',
    quota_too_many_windows: 'quota.errorPayload',
    quota_duplicate_window: 'quota.errorPayload',
    quota_invalid_credit_payload: 'quota.errorPayload',
    quota_too_many_credits: 'quota.errorPayload',
    quota_incomplete_credit_payload: 'quota.errorPayload',
    quota_invalid_payload: 'quota.errorPayload',
    quota_upstream_error: 'quota.errorSupplier',
  };
  for (const [code, message] of Object.entries(expected)) {
    assert.equal(quotaReadErrorMessage(code), message);
  }
});

test('unknown quota failures never expose a raw supplier message or infer exhaustion', () => {
  for (const code of [undefined, null, '', 'new_supplier_error', 'Bearer fixture-only-sensitive-value']) {
    assert.equal(quotaReadErrorMessage(code), 'quota.readFailed');
  }
});
