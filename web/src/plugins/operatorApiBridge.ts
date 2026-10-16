import { api } from '../api.js';
import type { OperatorUiApiRequestV1 } from '../../operator-ui-sdk/index.js';

type SchemaObject = Record<string, unknown>;

function object(value: unknown): SchemaObject | undefined {
  return value !== null && typeof value === 'object' && !Array.isArray(value) ? value as SchemaObject : undefined;
}

function resolveSchema(document: SchemaObject, value: unknown, seen = new Set<string>()): SchemaObject | undefined {
  const schema = object(value);
  if (!schema) return undefined;
  if (typeof schema.$ref !== 'string') return schema;
  if (!schema.$ref.startsWith('#/') || seen.has(schema.$ref)) throw new Error('API schema reference is not supported');
  seen.add(schema.$ref);
  let target: unknown = document;
  for (const part of schema.$ref.slice(2).split('/')) target = object(target)?.[part.replaceAll('~1', '/').replaceAll('~0', '~')];
  if (!object(target)) throw new Error('API schema reference cannot be resolved');
  return resolveSchema(document, target, seen);
}

function declaresTenant(document: SchemaObject, value: unknown, depth = 0): boolean {
  if (depth > 32) throw new Error('API request schema is too deeply nested');
  const schema = resolveSchema(document, value);
  if (!schema) return false;
  if (Object.hasOwn(object(schema.properties) ?? {}, 'tenant_external_id')) return true;
  return ['allOf', 'oneOf', 'anyOf'].some((key) => Array.isArray(schema[key])
    && (schema[key] as unknown[]).some((part) => declaresTenant(document, part, depth + 1)));
}

function bodyUsesTenant(document: SchemaObject, pathname: string, method: string): boolean {
  const paths = object(document.paths);
  const candidates = Object.entries(paths ?? {}).filter(([template]) => {
    const parts = template.split('/');
    const actual = pathname.split('/');
    return parts.length === actual.length && parts.every((part, index) => /^\{[^{}]+\}$/.test(part) || part === actual[index]);
  });
  // Match concrete paths first, then the most specific template. Equal-rank
  // templates are ambiguous: never let document insertion order choose scope.
  const ranked = candidates.map((candidate) => ({
    candidate,
    literals: candidate[0].split('/').filter((part) => !/^\{[^{}]+\}$/.test(part)).length,
  })).sort((left, right) => right.literals - left.literals);
  if (ranked.length > 1 && ranked[0].literals === ranked[1].literals) {
    throw new Error('API operation path is ambiguous in the host discovery schema');
  }
  const match = ranked[0]?.candidate;
  const operation = resolveSchema(document, resolveSchema(document, match?.[1])?.[method.toLowerCase()]);
  if (!operation) throw new Error('API operation is absent from the host discovery schema');
  const requestBody = resolveSchema(document, operation.requestBody);
  const content = object(requestBody?.content);
  return declaresTenant(document, object(content?.['application/json'])?.schema);
}

/** Credentials and transport options are owned by the host, never by a plugin request. */
export function createOperatorApiBridge(credential: string, tenantExternalId: string) {
  let discoveredSchema: SchemaObject | undefined;
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
    if (url.pathname.startsWith('/internal/v1/') && !['GET', 'HEAD', 'OPTIONS'].includes(method)) {
      // The backend's JSON tenant defaults do not consult query parameters. Use
      // the published request schema so global endpoints with strict bodies keep
      // their original contract, while tenant-aware writes cannot default away.
      const document = discoveredSchema ?? await api<SchemaObject>('/internal/v1/api-schema', credential, {
        signal, redirect: 'error', credentials: 'same-origin',
      });
      // Cache completed reads only. An aborted discovery must remain retryable.
      if (!object(document.paths)) throw new Error('API discovery schema has no paths');
      discoveredSchema = document;
      if (bodyUsesTenant(document, url.pathname, method)) {
        if (!tenantExternalId) throw new Error('Tenant-scoped API requests require an active tenant');
        if (body !== undefined && !object(body)) throw new Error('Tenant-scoped API requests require an object body');
        body = { ...object(body), tenant_external_id: tenantExternalId };
      }
    }
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
