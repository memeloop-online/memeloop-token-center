import { useEffect, useState } from 'react';
import SchemaField from '@rjsf/core/lib/components/fields/SchemaField.js';
import type { FieldProps } from '@rjsf/utils';
import { ariaDescribedByIds, ErrorSchemaBuilder } from '@rjsf/utils';
import { SecretInput } from './SecretInput';
import { useI18n } from './i18n';

function SecretSchemaValue({ schema, formData, fieldPathId, onChange, onBlur, onFocus, disabled, readonly, required, autofocus, rawErrors, errorSchema }: FieldProps) {
  const { t } = useI18n();
  const id = fieldPathId.$id;
  const label = typeof schema.title === 'string' ? schema.title : String(fieldPathId.path.at(-1) ?? t('secret.value'));
  const stringValue = schema.type === 'string' || (Array.isArray(schema.type) && schema.type.includes('string')) || schema.format === 'password' || (schema.type === undefined && typeof schema.const === 'string');
  const formatted = formData === undefined || formData === null ? '' : stringValue ? String(formData) : JSON.stringify(formData);
  const [text, setText] = useState(formatted);
  const [invalid, setInvalid] = useState(false);
  const hasErrors = (value: unknown): boolean => Boolean(value && typeof value === 'object' && Object.entries(value).some(([key, child]) => key === '__errors' ? Array.isArray(child) && child.length > 0 : hasErrors(child)));
  const validationError = Boolean(rawErrors?.length) || hasErrors(errorSchema);
  useEffect(() => { setText(formatted); setInvalid(false); }, [formatted, id, stringValue]);
  function update(next: string) {
    setText(next);
    try {
      const value: unknown = next === '' ? undefined : stringValue ? next : JSON.parse(next);
      setInvalid(false);
      onChange(value, fieldPathId.path, undefined, id);
    } catch {
      setInvalid(true);
      onChange(formData, fieldPathId.path, new ErrorSchemaBuilder().addErrors(t('secret.invalidJson')).ErrorSchema, id);
    }
  }
  return <div className="schema-secret-field">
    <label htmlFor={id}>{label}{required ? ' *' : ''}</label>
    <SecretInput id={id} label={label} value={text} onChange={(event) => update(event.target.value)} disabled={disabled} readOnly={readonly} required={required} autoFocus={autofocus} aria-invalid={invalid || validationError} aria-describedby={`${ariaDescribedByIds(id)}${invalid || validationError ? ` ${id}-secret-error` : ''}`} onBlur={() => onBlur(id, formData)} onFocus={() => onFocus(id, formData)} />
    {(invalid || validationError) && <p id={`${id}-secret-error`} className="schema-field-error" role="alert">{t(invalid ? 'secret.invalidJson' : 'schemaError.invalidField')}</p>}
  </div>;
}

/** Use resolved provider annotations, including nested refs/oneOf and opaque
 * write-only objects; an adapter-state object is never a visible JSON editor. */
export function SecureSchemaField(props: FieldProps) {
  const schema = props.registry.schemaUtils.retrieveSchema(props.schema, props.formData);
  if (schema.writeOnly === true || schema.format === 'password') {
    return <SchemaField {...props} schema={schema} uiSchema={{ ...props.uiSchema, 'ui:field': SecretSchemaValue, 'ui:fieldReplacesAnyOrOneOf': true, 'ui:options': { ...props.uiSchema?.['ui:options'], label: false } }} />;
  }
  return <SchemaField {...props} />;
}
