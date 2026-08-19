import {
  createErrorHandler, deepEquals, toErrorSchema, unwrapErrorHandler, validationDataMerge,
  type CustomValidator, type ErrorTransformer, type RJSFSchema, type RJSFValidationError,
  type UiSchema, type ValidationData, type ValidatorType,
} from '@rjsf/utils';

type Schema = RJSFSchema | boolean;

function property(path: Array<string | number>) {
  return path.length ? `.${path.join('.')}` : '.';
}

function addError(errors: RJSFValidationError[], name: string, message: string, path: Array<string | number>, schemaPath: string, params: Record<string, unknown> = {}) {
  const at = property(path);
  errors.push({ name, message, property: at, schemaPath, params, stack: `${at} ${message}` });
}

function resolve(root: Schema, reference: string): Schema | undefined {
  if (!reference.startsWith('#/')) return undefined;
  let value: unknown = root;
  for (const raw of reference.slice(2).split('/')) {
    const key = raw.replaceAll('~1', '/').replaceAll('~0', '~');
    if (!value || typeof value !== 'object' || Array.isArray(value)) return undefined;
    value = (value as Record<string, unknown>)[key];
  }
  return typeof value === 'boolean' || (value !== null && typeof value === 'object') ? value as Schema : undefined;
}

function matchesType(type: string, value: unknown) {
  if (type === 'null') return value === null;
  if (type === 'array') return Array.isArray(value);
  if (type === 'object') return value !== null && typeof value === 'object' && !Array.isArray(value);
  if (type === 'integer') return typeof value === 'number' && Number.isInteger(value);
  if (type === 'number') return typeof value === 'number' && Number.isFinite(value);
  return typeof value === type;
}

function validFormat(format: string, value: string) {
  const validUriCharacters = /^[\x21-\x7e]*$/u.test(value) && !/%(?![0-9a-f]{2})/iu.test(value);
  if (format === 'uri') {
    if (!validUriCharacters) return false;
    try { return Boolean(new URL(value)); } catch { return false; }
  }
  if (format === 'uri-reference') {
    if (!validUriCharacters) return false;
    try { new URL(value, 'https://schema.invalid/'); return true; } catch { return false; }
  }
  if (format === 'uuid') return /^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$/i.test(value);
  if (format === 'email') return /^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(value);
  if (format === 'date-time') return !Number.isNaN(Date.parse(value));
  return true;
}

function canonicalKey(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(canonicalKey).join(',')}]`;
  if (value !== null && typeof value === 'object') {
    return `{${Object.entries(value as Record<string, unknown>)
      .sort(([left], [right]) => left.localeCompare(right))
      .map(([key, item]) => `${JSON.stringify(key)}:${canonicalKey(item)}`)
      .join(',')}}`;
  }
  return JSON.stringify(value) ?? 'undefined';
}

function validPattern(pattern: string, value: string) {
  // Extension schemas are data, not trusted code. Reject regex constructs that
  // can create exponential backtracking on the browser's main thread.
  const unsafe = pattern.length > 256
    || value.length > 4096
    || /\\[1-9]|\(\?<?[=!]/u.test(pattern)
    || /\([^)]*(?:[+*]|\{\d)[^)]*\)(?:[+*]|\{\d)/u.test(pattern)
    || /\([^)]*\|[^)]*\)(?:[+*]|\{\d)/u.test(pattern);
  if (unsafe) return false;
  try { return new RegExp(pattern, 'u').test(value); } catch { return false; }
}

function validate(schema: Schema, value: unknown, root: Schema, path: Array<string | number>, schemaPath: string, errors: RJSFValidationError[], depth = 0): boolean {
  const before = errors.length;
  if (schema === true) return true;
  if (schema === false) { addError(errors, 'false schema', 'is not allowed', path, schemaPath); return false; }
  if (depth > 64) { addError(errors, '$ref', 'schema nesting is too deep', path, schemaPath); return false; }

  if (typeof schema.$ref === 'string') {
    const referenced = resolve(root, schema.$ref);
    if (!referenced) { addError(errors, '$ref', 'contains an unsupported schema reference', path, `${schemaPath}/$ref`); return false; }
    validate(referenced, value, root, path, schema.$ref, errors, depth + 1);
  }
  if (value === undefined) return true;

  const types = Array.isArray(schema.type) ? schema.type : schema.type ? [schema.type] : [];
  if (types.length && !types.some((type) => matchesType(String(type), value))) {
    addError(errors, 'type', `must be ${types.join(' or ')}`, path, `${schemaPath}/type`, { type: schema.type });
    return false;
  }
  if (schema.const !== undefined && !deepEquals(value, schema.const)) addError(errors, 'const', 'must equal the configured value', path, `${schemaPath}/const`, { allowedValue: schema.const });
  if (schema.enum && !schema.enum.some((item) => deepEquals(value, item))) addError(errors, 'enum', 'must be one of the allowed values', path, `${schemaPath}/enum`);

  schema.allOf?.forEach((entry, index) => validate(entry, value, root, path, `${schemaPath}/allOf/${index}`, errors, depth + 1));
  if (schema.anyOf && !schema.anyOf.some((entry, index) => validate(entry, value, root, path, `${schemaPath}/anyOf/${index}`, [], depth + 1))) addError(errors, 'anyOf', 'must match at least one allowed shape', path, `${schemaPath}/anyOf`);
  if (schema.oneOf && schema.oneOf.filter((entry, index) => validate(entry, value, root, path, `${schemaPath}/oneOf/${index}`, [], depth + 1)).length !== 1) addError(errors, 'oneOf', 'must match exactly one allowed shape', path, `${schemaPath}/oneOf`);
  if (schema.not && validate(schema.not, value, root, path, `${schemaPath}/not`, [], depth + 1)) addError(errors, 'not', 'matches a forbidden shape', path, `${schemaPath}/not`);
  if (schema.if) {
    const condition = validate(schema.if, value, root, path, `${schemaPath}/if`, [], depth + 1);
    const branch = condition ? schema.then : schema.else;
    if (branch) validate(branch, value, root, path, `${schemaPath}/${condition ? 'then' : 'else'}`, errors, depth + 1);
  }

  if (typeof value === 'string') {
    const length = [...value].length;
    if (schema.minLength !== undefined && length < schema.minLength) addError(errors, 'minLength', `must contain at least ${schema.minLength} characters`, path, `${schemaPath}/minLength`, { limit: schema.minLength });
    if (schema.maxLength !== undefined && length > schema.maxLength) addError(errors, 'maxLength', `must contain at most ${schema.maxLength} characters`, path, `${schemaPath}/maxLength`, { limit: schema.maxLength });
    if (schema.pattern) {
      if (!validPattern(schema.pattern, value)) addError(errors, 'pattern', 'has an invalid or unsafe format constraint', path, `${schemaPath}/pattern`);
    }
    if (schema.format && !validFormat(schema.format, value)) addError(errors, 'format', `must be a valid ${schema.format}`, path, `${schemaPath}/format`, { format: schema.format });
  }
  if (typeof value === 'number') {
    if (schema.minimum !== undefined && value < schema.minimum) addError(errors, 'minimum', `must be at least ${schema.minimum}`, path, `${schemaPath}/minimum`, { limit: schema.minimum });
    if (schema.maximum !== undefined && value > schema.maximum) addError(errors, 'maximum', `must be at most ${schema.maximum}`, path, `${schemaPath}/maximum`, { limit: schema.maximum });
    if (schema.exclusiveMinimum !== undefined && value <= Number(schema.exclusiveMinimum)) addError(errors, 'exclusiveMinimum', `must be greater than ${schema.exclusiveMinimum}`, path, `${schemaPath}/exclusiveMinimum`, { limit: schema.exclusiveMinimum });
    if (schema.exclusiveMaximum !== undefined && value >= Number(schema.exclusiveMaximum)) addError(errors, 'exclusiveMaximum', `must be less than ${schema.exclusiveMaximum}`, path, `${schemaPath}/exclusiveMaximum`, { limit: schema.exclusiveMaximum });
    if (schema.multipleOf !== undefined && Math.abs(value / schema.multipleOf - Math.round(value / schema.multipleOf)) > 1e-9) addError(errors, 'multipleOf', `must be a multiple of ${schema.multipleOf}`, path, `${schemaPath}/multipleOf`, { limit: schema.multipleOf });
  }
  if (Array.isArray(value)) {
    if (schema.minItems !== undefined && value.length < schema.minItems) addError(errors, 'minItems', `must contain at least ${schema.minItems} items`, path, `${schemaPath}/minItems`, { limit: schema.minItems });
    if (schema.maxItems !== undefined && value.length > schema.maxItems) addError(errors, 'maxItems', `must contain at most ${schema.maxItems} items`, path, `${schemaPath}/maxItems`, { limit: schema.maxItems });
    if (schema.uniqueItems && new Set(value.map(canonicalKey)).size !== value.length) addError(errors, 'uniqueItems', 'must not contain duplicate items', path, `${schemaPath}/uniqueItems`);
    if (schema.items && !Array.isArray(schema.items)) value.forEach((item, index) => validate(schema.items as Schema, item, root, [...path, index], `${schemaPath}/items`, errors, depth + 1));
  }
  if (value !== null && typeof value === 'object' && !Array.isArray(value)) {
    const object = value as Record<string, unknown>;
    const entries = Object.entries(object);
    if (schema.minProperties !== undefined && entries.length < schema.minProperties) addError(errors, 'minProperties', `must contain at least ${schema.minProperties} properties`, path, `${schemaPath}/minProperties`, { limit: schema.minProperties });
    if (schema.maxProperties !== undefined && entries.length > schema.maxProperties) addError(errors, 'maxProperties', `must contain at most ${schema.maxProperties} properties`, path, `${schemaPath}/maxProperties`, { limit: schema.maxProperties });
    for (const required of schema.required ?? []) if (!(required in object)) addError(errors, 'required', 'is required', [...path, required], `${schemaPath}/required`, { missingProperty: required });
    if (schema.propertyNames !== undefined) for (const key of Object.keys(object)) validate(schema.propertyNames as Schema, key, root, [...path, key], `${schemaPath}/propertyNames`, errors, depth + 1);
    for (const [key, child] of Object.entries(schema.properties ?? {})) if (object[key] !== undefined) validate(child as Schema, object[key], root, [...path, key], `${schemaPath}/properties/${key}`, errors, depth + 1);
    if (schema.additionalProperties === false) {
      for (const key of Object.keys(object)) if (!(key in (schema.properties ?? {}))) addError(errors, 'additionalProperties', 'is not an allowed field', [...path, key], `${schemaPath}/additionalProperties`);
    } else if (schema.additionalProperties && typeof schema.additionalProperties === 'object') {
      for (const [key, item] of Object.entries(object)) if (!(key in (schema.properties ?? {}))) validate(schema.additionalProperties as Schema, item, root, [...path, key], `${schemaPath}/additionalProperties`, errors, depth + 1);
    }
  }
  return errors.length === before;
}

class SafeValidator implements ValidatorType {
  rawValidation<Result = RJSFValidationError>(schema: RJSFSchema, formData?: unknown) {
    const errors: RJSFValidationError[] = [];
    validate(schema, formData, schema, [], '#', errors);
    return { errors: errors as Result[] };
  }

  isValid(schema: RJSFSchema, formData: unknown, rootSchema: RJSFSchema) {
    return validate(schema, formData, rootSchema, [], '#', []);
  }

  validateFormData(formData: unknown, schema: RJSFSchema, customValidate?: CustomValidator, transformErrors?: ErrorTransformer, uiSchema?: UiSchema): ValidationData<unknown> {
    let errors = this.rawValidation<RJSFValidationError>(schema, formData).errors ?? [];
    if (transformErrors) errors = transformErrors(errors, uiSchema);
    const result = { errors, errorSchema: toErrorSchema(errors) };
    if (!customValidate) return result;
    const custom = customValidate(formData, createErrorHandler(formData), uiSchema, result.errorSchema);
    return validationDataMerge(result, unwrapErrorHandler(custom));
  }
}

export const safeValidator = new SafeValidator();
