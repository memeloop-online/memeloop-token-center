import assert from 'node:assert/strict';
import test from 'node:test';
import { isExpectedResourceAbort } from './support/request-failures.js';

test('accepts only the debounced model catalog GET abort', () => {
  assert.equal(isExpectedResourceAbort(
    'GET',
    'http://127.0.0.1:41739/internal/v1/upstream-models?tenant_external_id=e2e&q=model',
    'net::ERR_ABORTED',
  ), true);
});

test('does not hide other aborted or failed browser requests', () => {
  assert.equal(isExpectedResourceAbort('POST', 'http://127.0.0.1/internal/v1/upstream-models', 'net::ERR_ABORTED'), false);
  assert.equal(isExpectedResourceAbort('GET', 'http://127.0.0.1/internal/v1/upstream-models/sync', 'net::ERR_ABORTED'), false);
  assert.equal(isExpectedResourceAbort('GET', 'http://127.0.0.1/internal/v1/upstreams', 'net::ERR_ABORTED'), false);
  assert.equal(isExpectedResourceAbort('GET', 'http://127.0.0.1/internal/v1/upstream-models', 'net::ERR_FAILED'), false);
});

test('monitoring scope cleanup permits only an aborted snapshot GET', () => {
  const url = 'http://127.0.0.1/internal/v1/monitoring-snapshot?scope=tenant&tenant_external_id=e2e';
  assert.equal(isExpectedResourceAbort('GET', url, 'net::ERR_ABORTED'), true);
  assert.equal(isExpectedResourceAbort('POST', url, 'net::ERR_ABORTED'), false);
  assert.equal(isExpectedResourceAbort('GET', url, 'net::ERR_CONNECTION_RESET'), false);
  assert.equal(isExpectedResourceAbort('GET', `${url.split('?')[0]}/other`, 'net::ERR_ABORTED'), false);
});
