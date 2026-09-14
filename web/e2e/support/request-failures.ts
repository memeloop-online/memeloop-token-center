export function isExpectedResourceAbort(method: string, requestUrl: string, failure: string): boolean {
  if (method !== 'GET' || failure !== 'net::ERR_ABORTED') return false;
  try {
    const path = new URL(requestUrl).pathname;
    return path === '/internal/v1/upstream-models' || path === '/internal/v1/monitoring-snapshot';
  } catch {
    return false;
  }
}
