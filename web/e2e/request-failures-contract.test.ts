import assert from 'node:assert/strict';
import test from 'node:test';
import { isExpectedViewReadAbort } from './support/request-failures.js';

test('accepts only documented client-cancelled view GETs', () => {
  assert.equal(isExpectedViewReadAbort(
    'GET',
    'http://127.0.0.1:41739/internal/v1/upstream-models?tenant_external_id=e2e&q=model',
    'net::ERR_ABORTED',
  ), true);
  assert.equal(isExpectedViewReadAbort(
    'GET',
    'http://127.0.0.1:41739/internal/v1/model-prices?currency=USD',
    'net::ERR_ABORTED',
  ), true);
  assert.equal(isExpectedViewReadAbort(
    'GET',
    'http://127.0.0.1:41739/internal/v1/generation-prices?currency=USD',
    'net::ERR_ABORTED',
  ), true);
  assert.equal(isExpectedViewReadAbort(
    'GET',
    'http://127.0.0.1:41739/internal/v1/model-prices/usage-summary?tenant_external_id=e2e',
    'net::ERR_ABORTED',
  ), true);
});

test('does not hide other aborted or failed browser requests', () => {
  assert.equal(isExpectedViewReadAbort('POST', 'http://127.0.0.1/internal/v1/upstream-models', 'net::ERR_ABORTED'), false);
  assert.equal(isExpectedViewReadAbort('GET', 'http://127.0.0.1/internal/v1/model-prices/browser-model', 'net::ERR_ABORTED'), false);
  assert.equal(isExpectedViewReadAbort('GET', 'http://127.0.0.1/internal/v1/upstreams', 'net::ERR_ABORTED'), false);
  assert.equal(isExpectedViewReadAbort('GET', 'http://127.0.0.1/internal/v1/generation-prices', 'net::ERR_FAILED'), false);
});
