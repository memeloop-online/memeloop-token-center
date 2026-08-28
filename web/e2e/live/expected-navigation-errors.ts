import assert from 'node:assert/strict';
import { createHmac, randomBytes } from 'node:crypto';

type SafePageErrorName =
  | 'AggregateError'
  | 'DOMException'
  | 'Error'
  | 'EvalError'
  | 'RangeError'
  | 'ReferenceError'
  | 'SyntaxError'
  | 'TypeError'
  | 'URIError'
  | 'UnknownError';

type SafeRequestFailureCode =
  | 'net::ERR_ABORTED'
  | 'net::ERR_BLOCKED_BY_CLIENT'
  | 'net::ERR_CERT_AUTHORITY_INVALID'
  | 'net::ERR_CERT_COMMON_NAME_INVALID'
  | 'net::ERR_CERT_DATE_INVALID'
  | 'net::ERR_CONNECTION_CLOSED'
  | 'net::ERR_CONNECTION_REFUSED'
  | 'net::ERR_CONNECTION_RESET'
  | 'net::ERR_FAILED'
  | 'net::ERR_NAME_NOT_RESOLVED'
  | 'net::ERR_TIMED_OUT'
  | 'unknown';

type SafeOriginMarker = 'configured-origin' | 'unconfigured-origin' | 'unknown-origin';

export type BrowserFailure =
  | {
    kind: 'console';
    category: 'http-client-error';
    status: number;
    sourceOrigin: SafeOriginMarker;
    sourceFingerprint: string;
  }
  | { kind: 'console'; category: 'other'; sourceOrigin: SafeOriginMarker }
  | { kind: 'page'; name: SafePageErrorName }
  | { kind: 'request'; method: string; origin: SafeOriginMarker; failure: SafeRequestFailureCode };

interface URLIdentity {
  fingerprint: string;
  origin: string;
}

interface VerifiedClientErrorNavigation {
  fingerprint: string;
  status: number;
}

const safePageErrorNames = new Set<SafePageErrorName>([
  'AggregateError',
  'DOMException',
  'Error',
  'EvalError',
  'RangeError',
  'ReferenceError',
  'SyntaxError',
  'TypeError',
  'URIError',
]);

const safeRequestFailureCodes = new Set<SafeRequestFailureCode>([
  'net::ERR_ABORTED',
  'net::ERR_BLOCKED_BY_CLIENT',
  'net::ERR_CERT_AUTHORITY_INVALID',
  'net::ERR_CERT_COMMON_NAME_INVALID',
  'net::ERR_CERT_DATE_INVALID',
  'net::ERR_CONNECTION_CLOSED',
  'net::ERR_CONNECTION_REFUSED',
  'net::ERR_CONNECTION_RESET',
  'net::ERR_FAILED',
  'net::ERR_NAME_NOT_RESOLVED',
  'net::ERR_TIMED_OUT',
]);

const safeHTTPMethods = new Set([
  'CONNECT', 'DELETE', 'GET', 'HEAD', 'OPTIONS', 'PATCH', 'POST', 'PUT', 'TRACE',
]);

const urlFingerprintKey = randomBytes(32);

export class ExpectedClientErrorNavigationLedger {
  private readonly verifiedNavigations: VerifiedClientErrorNavigation[] = [];

  verify(
    requestedURL: string,
    expectedStatus: number,
    actualURL: string,
    actualStatus: number,
  ): void {
    assert.ok(
      expectedStatus >= 400 && expectedStatus < 500,
      `expected navigation status must be a 4xx response, received ${expectedStatus}`,
    );
    const requestedIdentity = identifyURL(requestedURL);
    const actualIdentity = identifyURL(actualURL);
    assert.ok(requestedIdentity, 'expected navigation requested URL must be valid');
    assert.ok(actualIdentity, 'expected navigation response URL must be valid');
    assert.ok(
      actualIdentity.fingerprint === requestedIdentity.fingerprint,
      'expected navigation response URL to match the requested URL',
    );
    assert.equal(actualStatus, expectedStatus, `expected navigation must return HTTP ${expectedStatus}`);
    this.verifiedNavigations.push({
      fingerprint: requestedIdentity.fingerprint,
      status: expectedStatus,
    });
  }

  unexpectedFailures(failures: readonly BrowserFailure[]): BrowserFailure[] {
    const availableNavigations = [...this.verifiedNavigations];
    return failures.filter((failure) => {
      if (failure.kind !== 'console' || failure.category !== 'http-client-error') return true;
      const matchingNavigation = availableNavigations.findIndex((navigation) => (
        navigation.status === failure.status
          && navigation.fingerprint === failure.sourceFingerprint
      ));
      if (matchingNavigation === -1) return true;
      availableNavigations.splice(matchingNavigation, 1);
      return false;
    });
  }
}

export function captureConsoleFailure(
  message: string,
  sourceURL: string,
  pageURL: string,
  reportableOrigins: ReadonlySet<string> = new Set(),
): BrowserFailure {
  const sourceIdentity = sourceURL ? identifyURL(sourceURL) : identifyURL(pageURL);
  const status = clientErrorStatus(message);
  if (status !== undefined) {
    return {
      kind: 'console',
      category: 'http-client-error',
      status,
      sourceOrigin: reportableOrigin(sourceIdentity, reportableOrigins),
      sourceFingerprint: sourceIdentity?.fingerprint ?? 'invalid-url',
    };
  }
  return {
    kind: 'console',
    category: 'other',
    sourceOrigin: reportableOrigin(sourceIdentity, reportableOrigins),
  };
}

export function capturePageFailure(name: string): BrowserFailure {
  return {
    kind: 'page',
    name: safePageErrorNames.has(name as SafePageErrorName)
      ? name as SafePageErrorName
      : 'UnknownError',
  };
}

export function captureRequestFailure(
  method: string,
  requestURL: string,
  errorText: string | undefined,
  reportableOrigins: ReadonlySet<string> = new Set(),
): BrowserFailure {
  const normalizedMethod = method.toUpperCase();
  return {
    kind: 'request',
    method: safeHTTPMethods.has(normalizedMethod) ? normalizedMethod : 'OTHER',
    origin: reportableOrigin(identifyURL(requestURL), reportableOrigins),
    failure: safeRequestFailureCodes.has(errorText as SafeRequestFailureCode)
      ? errorText as SafeRequestFailureCode
      : 'unknown',
  };
}

function clientErrorStatus(message: string): number | undefined {
  const match = /^Failed to load resource: the server responded with a status of (4\d{2})(?: \([^\r\n]*\))?$/.exec(message);
  return match ? Number(match[1]) : undefined;
}

function identifyURL(value: string): URLIdentity | undefined {
  try {
    const url = new URL(value);
    return {
      fingerprint: createHmac('sha256', urlFingerprintKey).update(url.href).digest('hex'),
      origin: url.origin === 'null' ? 'opaque-origin' : url.origin,
    };
  } catch {
    return undefined;
  }
}

function reportableOrigin(
  identity: URLIdentity | undefined,
  reportableOrigins: ReadonlySet<string>,
): SafeOriginMarker {
  if (!identity) return 'unknown-origin';
  return reportableOrigins.has(identity.origin) ? 'configured-origin' : 'unconfigured-origin';
}
