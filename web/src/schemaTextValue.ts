export function schemaTextValue(value: unknown, defaultValue: unknown, emptyValue: unknown): string {
  if (value === undefined && emptyValue === '' && typeof defaultValue === 'string') return defaultValue;
  return value == null ? '' : String(value);
}
