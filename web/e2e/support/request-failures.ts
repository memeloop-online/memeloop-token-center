export function isExpectedModelCatalogAbort(method: string, requestUrl: string, failure: string): boolean {
  if (!failure.includes('ERR_ABORTED')) return false;
  try {
    const path = new URL(requestUrl).pathname;
    return (method === 'GET' && path === '/internal/v1/upstream-models')
      || (method === 'POST' && path === '/internal/v1/upstream-models/query');
  } catch {
    return false;
  }
}
