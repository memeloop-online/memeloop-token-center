export function isExpectedModelCatalogAbort(method: string, requestUrl: string, failure: string): boolean {
  if (method !== 'GET' || !failure.includes('ERR_ABORTED')) return false;
  try {
    return new URL(requestUrl).pathname === '/internal/v1/upstream-models';
  } catch {
    return false;
  }
}
