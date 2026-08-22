import type { ProviderType } from '../types.js';

type JsonSchema = Record<string, unknown>;

function objectValue(value: unknown): JsonSchema | undefined {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    ? value as JsonSchema
    : undefined;
}

function credentialKind(schema: JsonSchema): string | undefined {
  const properties = objectValue(schema.properties);
  const type = objectValue(properties?.type);
  return typeof type?.const === 'string' ? type.const : undefined;
}

/**
 * Build the direct-connection credential schema advertised by a provider.
 * Interactive OAuth is provisioned through its authorization flow, while any
 * API-key or unauthenticated variants remain first-class direct methods.
 */
export function directCredentialSchema(source: Record<string, unknown>): JsonSchema | undefined {
  const schema = structuredClone(source) as JsonSchema;
  if (!Array.isArray(schema.oneOf)) {
    return credentialKind(schema) === 'oauth' ? undefined : schema;
  }
  const directVariants = schema.oneOf
    .map(objectValue)
    .filter((variant): variant is JsonSchema => variant !== undefined && credentialKind(variant) !== 'oauth');
  return directVariants.length > 0 ? { ...schema, oneOf: directVariants } : undefined;
}

export function supportsDirectConnection(provider: ProviderType): boolean {
  return directCredentialSchema(provider.credential_schema) !== undefined;
}
