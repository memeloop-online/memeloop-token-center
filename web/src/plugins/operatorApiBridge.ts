import { api } from '../api.js';
import type { OperatorUiApiRequestV1 } from '../../operator-ui-sdk/index.js';

/** Credentials and transport options are owned by the host, never by a plugin request. */
export function createOperatorApiBridge(credential: string, tenantExternalId: string) {
  return async function requestApi<T = unknown>(request: OperatorUiApiRequestV1): Promise<T> {
    const { path, signal } = request;
    // Reject ambiguous URL forms before URL normalization can reinterpret them.
    if (typeof path !== 'string' || (!/^\/(?:internal|self)\/v1\//.test(path) && !/^\/v1\//.test(path))
      || /[\\\s#]/.test(path)) {
      throw new Error('API path must be an absolute same-origin API path');
    }
    const url = new URL(path, 'https://operator.invalid');
    if (url.origin !== 'https://operator.invalid' || url.username || url.password
      || url.pathname !== path.split('?')[0]) {
      throw new Error('API path must not contain URL traversal or credentials');
    }
    const method = request.method ?? 'GET';
    if (!['GET', 'POST', 'PUT', 'PATCH', 'DELETE', 'HEAD', 'OPTIONS'].includes(method)) {
      throw new Error('Unsupported API method');
    }
    // A request may select resources, but cannot replace the active tenant scope.
    if (tenantExternalId) url.searchParams.set('tenant_external_id', tenantExternalId);
    else if (url.searchParams.has('tenant_external_id')) throw new Error('API request cannot replace the active tenant');
    let body = request.body;
    if (body && typeof body === 'object' && !Array.isArray(body) && Object.hasOwn(body, 'tenant_external_id')) {
      if (!tenantExternalId) throw new Error('API request cannot replace the active tenant');
      body = { ...body, tenant_external_id: tenantExternalId };
    }
    if ((method === 'GET' || method === 'HEAD') && body !== undefined) throw new Error('Read requests cannot include a body');
    return api<T>(`${url.pathname}${url.search}`, credential, {
      method,
      ...(body === undefined ? {} : { body: JSON.stringify(body) }),
      signal,
      redirect: 'error',
      credentials: 'same-origin',
    });
  };
}
