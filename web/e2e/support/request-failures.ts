export function isExpectedResourceAbort(method: string, requestUrl: string, failure: string): boolean {
  if (failure !== 'net::ERR_ABORTED') return false;
  try {
    const path = new URL(requestUrl).pathname;
    if (method === 'POST') return path === '/internal/v1/requests/query';
    return method === 'GET' && (path === '/internal/v1/upstream-models' || path === '/internal/v1/monitoring-snapshot');
  } catch {
    return false;
  }
}
