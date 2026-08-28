import assert from 'node:assert/strict';
import test from 'node:test';

import {
  captureConsoleFailure,
  capturePageFailure,
  captureRequestFailure,
  ExpectedClientErrorNavigationLedger,
  type BrowserFailure,
} from './expected-navigation-errors.js';

const operatorURL = 'https://gateway.example.test/operator';
const reportableOrigins = new Set([
  'https://control.example.test',
  'https://gateway.example.test',
]);
const operator404 = captureConsoleFailure(
  'Failed to load resource: the server responded with a status of 404 (Not Found)',
  operatorURL,
  operatorURL,
  reportableOrigins,
);

test('a verified expected 4xx navigation suppresses one exact matching Chromium console error', () => {
  const ledger = new ExpectedClientErrorNavigationLedger();
  ledger.verify(operatorURL, 404, operatorURL, 404);

  assert.deepEqual(ledger.unexpectedFailures([operator404]), []);
  assert.deepEqual(ledger.unexpectedFailures([operator404, operator404]), [operator404]);
});

test('an expected navigation must be a verified exact URL and exact 4xx status', () => {
  const wrongStatus = new ExpectedClientErrorNavigationLedger();
  assert.throws(
    () => wrongStatus.verify(operatorURL, 404, operatorURL, 403),
    /must return HTTP 404/,
  );
  assert.deepEqual(wrongStatus.unexpectedFailures([operator404]), [operator404]);

  const redirected = new ExpectedClientErrorNavigationLedger();
  assert.throws(() => redirected.verify(
    operatorURL,
    404,
    'https://control.example.test/operator',
    404,
  ), /expected navigation response URL to match/);
  assert.deepEqual(redirected.unexpectedFailures([operator404]), [operator404]);

  assert.throws(() => new ExpectedClientErrorNavigationLedger().verify(
    'https://control.example.test/operator',
    500,
    'https://control.example.test/operator',
    500,
  ), /must be a 4xx response/);
});

test('undeclared, cross-URL, non-network, page, and request failures remain strict', () => {
  const ledger = new ExpectedClientErrorNavigationLedger();
  ledger.verify(operatorURL, 404, operatorURL, 404);
  const failures: BrowserFailure[] = [
    captureConsoleFailure(
      'Failed to load resource: the server responded with a status of 403 (Forbidden)',
      'https://control.example.test/operator',
      'https://control.example.test/operator',
      reportableOrigins,
    ),
    captureConsoleFailure(
      'Failed to load resource: the server responded with a status of 404 (Not Found)',
      'https://gateway.example.test/internal/v1/tenants',
      operatorURL,
      reportableOrigins,
    ),
    captureConsoleFailure('application reported HTTP 404', operatorURL, operatorURL, reportableOrigins),
    capturePageFailure('TypeError'),
    captureRequestFailure('GET', operatorURL, 'net::ERR_CONNECTION_RESET', reportableOrigins),
  ];

  assert.deepEqual(ledger.unexpectedFailures(failures), failures);
});

test('a missing Chromium console location falls back only to the exact current navigation URL', () => {
  const ledger = new ExpectedClientErrorNavigationLedger();
  ledger.verify(operatorURL, 404, operatorURL, 404);
  const navigationFailure = captureConsoleFailure(
    'Failed to load resource: the server responded with a status of 404 ()',
    '',
    operatorURL,
    reportableOrigins,
  );
  const unrelatedFailure = captureConsoleFailure(
    'Failed to load resource: the server responded with a status of 404 ()',
    '',
    'https://gateway.example.test/internal/v1/tenants',
    reportableOrigins,
  );

  assert.deepEqual(ledger.unexpectedFailures([navigationFailure]), []);
  assert.deepEqual(ledger.unexpectedFailures([unrelatedFailure]), [unrelatedFailure]);
});

test('captured browser failures never retain free text, URL credentials, paths, or queries', () => {
  const canary = 'provider_canary_must_not_escape';
  const sensitiveOrigin = `https://${canary}.example.test`;
  const failures: BrowserFailure[] = [
    captureConsoleFailure(
      `console payload ${canary}`,
      `https://${canary}:password@gateway.example.test/private/${canary}?token=${canary}`,
      `https://gateway.example.test/portal?token=${canary}`,
      reportableOrigins,
    ),
    captureConsoleFailure(
      `Failed to load resource: the server responded with a status of 404 (${canary})`,
      `https://gateway.example.test/private/${canary}?token=${canary}`,
      `https://gateway.example.test/portal?token=${canary}`,
      reportableOrigins,
    ),
    capturePageFailure(canary),
    captureRequestFailure(
      canary,
      `https://${canary}:password@gateway.example.test/private/${canary}?token=${canary}`,
      `net::ERR_FAILED ${canary}`,
      reportableOrigins,
    ),
    captureRequestFailure(
      'GET',
      `https://gateway.example.test.${canary}/operator`,
      'net::ERR_FAILED',
      reportableOrigins,
    ),
    captureRequestFailure(
      'GET',
      `${sensitiveOrigin}/operator`,
      'net::ERR_FAILED',
      new Set([sensitiveOrigin]),
    ),
  ];
  const serialized = JSON.stringify(failures);

  assert.ok(!serialized.includes(canary));
  assert.ok(!serialized.includes('/private/'));
  assert.ok(!serialized.includes('?token='));
  assert.ok(!serialized.includes('password'));
  assert.deepEqual(failures[2], { kind: 'page', name: 'UnknownError' });
  assert.deepEqual(failures[3], {
    kind: 'request',
    method: 'OTHER',
    origin: 'configured-origin',
    failure: 'unknown',
  });
  assert.deepEqual(failures[4], {
    kind: 'request',
    method: 'GET',
    origin: 'unconfigured-origin',
    failure: 'net::ERR_FAILED',
  });
  assert.deepEqual(failures[5], {
    kind: 'request',
    method: 'GET',
    origin: 'configured-origin',
    failure: 'net::ERR_FAILED',
  });
});
