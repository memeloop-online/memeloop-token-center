const EXPECTED_ABORTED_READ_PATHS = new Set([
  '/internal/v1/upstream-models',
  '/internal/v1/model-prices',
  '/internal/v1/generation-prices',
  '/internal/v1/model-prices/usage-summary',
]);

/**
 * React cancels an in-flight read when its owning view is replaced. Keep this
 * exception exact: mutations, other paths, and non-abort failures still fail.
 */
export function isExpectedViewReadAbort(method: string, requestUrl: string, failure: string): boolean {
  if (method !== 'GET' || !failure.includes('ERR_ABORTED')) return false;
  try {
    return EXPECTED_ABORTED_READ_PATHS.has(new URL(requestUrl).pathname);
  } catch {
    return false;
  }
}
