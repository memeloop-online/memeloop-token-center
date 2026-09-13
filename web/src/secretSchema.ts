import { createSchemaUtils, type RJSFSchema, type ValidatorType } from '@rjsf/utils';

function isSecretSchema(node: Record<string, unknown>, root: RJSFSchema, seen = new Set<unknown>()): boolean {
  if (seen.has(node)) return false;
  seen.add(node);
  if (node.writeOnly === true || node.format === 'password') return true;
  if (typeof node.$ref === 'string' && node.$ref.startsWith('#/')) {
    let target: unknown = root;
    for (const part of node.$ref.slice(2).split('/')) target = target && typeof target === 'object' ? (target as Record<string, unknown>)[part.replace(/~1/g, '/').replace(/~0/g, '~')] : undefined;
    if (target && typeof target === 'object' && isSecretSchema(target as Record<string, unknown>, root, seen)) return true;
  }
  return Array.isArray(node.allOf) && node.allOf.some((part) => part && typeof part === 'object' && isSecretSchema(part, root, seen));
}

function withoutSecretDefaults(schema: RJSFSchema): RJSFSchema {
  const visit = (value: unknown, inherited = false): unknown => {
    if (Array.isArray(value)) return value.map((item) => visit(item, inherited));
    if (!value || typeof value !== 'object') return value;
    const node = value as Record<string, unknown>;
    const secret = inherited || isSecretSchema(node, schema);
    return Object.fromEntries(Object.entries(node)
      .filter(([key]) => !secret || (key !== 'default' && key !== 'examples'))
      .map(([key, child]) => [key, ['default', 'examples', 'const', 'enum'].includes(key) ? child : visit(child, secret)]));
  };
  return visit(schema) as RJSFSchema;
}

/** Prepare the initial form once, before RJSF can compute defaults. Existing
 * secrets are absent, not blank replacements; only subsequent user input may
 * populate them. The server preserves omitted secret paths under its CAS. */
export function prepareSecretForm(schema: RJSFSchema, validator: ValidatorType, existing?: unknown) {
  const root = withoutSecretDefaults(schema);
  const utils = createSchemaUtils(validator, root);
  function visit(input: RJSFSchema, value: unknown, depth = 0): { schema: RJSFSchema; data: unknown; secret: boolean } {
    if (depth > 32) throw new Error('Unsupported credential schema depth');
    const resolved = utils.retrieveSchema(input, value);
    const secret = isSecretSchema(resolved, root);
    const clean = withoutSecretDefaults(resolved);
    if (secret) return { schema: clean, data: undefined, secret: true };
    let data = value && typeof value === 'object' && !Array.isArray(value) ? { ...value as Record<string, unknown> } : value;
    if (clean.properties) {
      clean.properties = { ...clean.properties };
      for (const [key, child] of Object.entries(clean.properties)) {
        if (typeof child === 'boolean') continue;
        const original = data && typeof data === 'object' ? (data as Record<string, unknown>)[key] : undefined;
        const next = visit(child, original, depth + 1);
        clean.properties[key] = next.schema;
        if (data && typeof data === 'object') {
          if (next.data === undefined) delete (data as Record<string, unknown>)[key];
          else (data as Record<string, unknown>)[key] = next.data;
        }
        if (next.secret && existing !== undefined) clean.required = clean.required?.filter((item) => item !== key);
      }
    }
    for (const key of ['oneOf', 'anyOf'] as const) {
      if (clean[key]) clean[key] = clean[key].map((child) => {
        const next = visit(child, data, depth + 1);
        data = next.data;
        return next.schema;
      });
    }
    // Object defaults can contain secret descendants even when the object
    // itself is not writeOnly. Refuse aggregate defaults; leaf defaults remain.
    if (clean.properties || clean.oneOf || clean.anyOf) { delete clean.default; delete clean.examples; }
    return { schema: clean, data, secret: false };
  }
  const prepared = visit(root, existing);
  return { schema: prepared.schema, formData: prepared.data };
}
