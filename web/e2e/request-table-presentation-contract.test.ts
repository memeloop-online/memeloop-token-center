import assert from 'node:assert/strict';
import test from 'node:test';
import { averageRequestOutputTps, nonCachedRequestInput, requestCredentialLabel } from '../src/requestTablePresentation';
import type { RequestView } from '../src/types';

const request: RequestView = {
  request_id: 'fixture', created_at: 1, protocol: 'openai', model: 'fixture', status_code: 200,
  duration_ms: 2000, input_tokens: 160, cached_input_tokens: 40, cache_write_tokens: 20,
  output_tokens: 32, cost: '0', error_code: null,
};

test('uncached input uses both recorded cache components and does not fabricate missing counts', () => {
  assert.equal(nonCachedRequestInput(request), 100);
  assert.equal(nonCachedRequestInput({ ...request, cached_input_tokens: undefined }), null);
  assert.equal(nonCachedRequestInput({ ...request, cache_write_tokens: undefined }), null);
  assert.equal(nonCachedRequestInput({ ...request, cached_input_tokens: 150 }), null);
  assert.equal(nonCachedRequestInput({ ...request, input_tokens: 0, cached_input_tokens: 0, cache_write_tokens: 0 }), 0);
});

test('average output TPS uses recorded total seconds, distinguishes valid zero from unavailable and running', () => {
  assert.equal(averageRequestOutputTps(request), 16);
  assert.equal(averageRequestOutputTps({ ...request, output_tokens: 0 }), 0);
  for (const duration_ms of [null, 0, -1, Number.NaN, Number.POSITIVE_INFINITY]) assert.equal(averageRequestOutputTps({ ...request, duration_ms }), null);
  assert.equal(averageRequestOutputTps({ ...request, status_code: null }), null);
  assert.equal(averageRequestOutputTps({ ...request, output_tokens: -1 }), null);
});

test('credential identity remains meaningful without inventing an alias for explicitly unbound history', () => {
  assert.equal(requestCredentialLabel(request, 'Portal key', 'en'), 'Portal key');
  assert.equal(requestCredentialLabel({ ...request, credential_identity: null }, 'Portal key', 'en'), 'Credential not recorded');
  assert.equal(requestCredentialLabel({ ...request, credential_identity: { tenant_external_id: 'fixture', key_id: 'key', key_alias: '', principal_external_id: 'user' } }, 'Portal key', 'en'), 'Unnamed credential');
});
