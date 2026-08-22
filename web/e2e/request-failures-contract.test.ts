import assert from 'node:assert/strict';
import test from 'node:test';
import { isExpectedModelCatalogAbort } from './support/request-failures.js';

test('accepts only the debounced model catalog GET abort', () => {
  assert.equal(isExpectedModelCatalogAbort(
    'GET',
    'http://127.0.0.1:41739/internal/v1/upstream-models?tenant_external_id=e2e&q=model',
    'net::ERR_ABORTED',
  ), true);
});

test('does not hide other aborted or failed browser requests', () => {
  assert.equal(isExpectedModelCatalogAbort('POST', 'http://127.0.0.1/internal/v1/upstream-models', 'net::ERR_ABORTED'), false);
  assert.equal(isExpectedModelCatalogAbort('GET', 'http://127.0.0.1/internal/v1/upstream-models/sync', 'net::ERR_ABORTED'), false);
  assert.equal(isExpectedModelCatalogAbort('GET', 'http://127.0.0.1/internal/v1/upstreams', 'net::ERR_ABORTED'), false);
  assert.equal(isExpectedModelCatalogAbort('GET', 'http://127.0.0.1/internal/v1/upstream-models', 'net::ERR_FAILED'), false);
});
