import type { ArrayFieldItemTemplateProps, ArrayFieldTemplateProps, ErrorListProps, FieldErrorProps, FieldProps, RJSFValidationError } from '@rjsf/utils';
import { useEffect, useState } from 'react';
import { useI18n } from './i18n';

function validationMessage(error: RJSFValidationError, t: (key: string, variables?: Record<string, string | number>) => string) {
  const parameters = error.params ?? {};
  const limit = String(parameters.limit ?? parameters.allowedValue ?? '');
  const expected = String(parameters.type ?? parameters.format ?? '');
  const messages: Record<string, string> = {
    required: 'schemaError.required',
    type: 'schemaError.type',
    const: 'schemaError.const',
    enum: 'schemaError.enum',
    minLength: 'schemaError.minimum',
    maxLength: 'schemaError.maximum',
    minimum: 'schemaError.minimum',
    maximum: 'schemaError.maximum',
    exclusiveMinimum: 'schemaError.greaterThan',
    exclusiveMaximum: 'schemaError.lessThan',
    multipleOf: 'schemaError.multipleOf',
    minItems: 'schemaError.minimum',
    maxItems: 'schemaError.maximum',
    minProperties: 'schemaError.minimum',
    maxProperties: 'schemaError.maximum',
    uniqueItems: 'schemaError.unique',
    format: 'schemaError.format',
    pattern: 'schemaError.pattern',
    additionalProperties: 'schemaError.additionalProperty',
    oneOf: 'schemaError.oneOf',
    anyOf: 'schemaError.anyOf',
    not: 'schemaError.not',
  };
  return t(messages[error.name ?? ''] ?? 'schemaError.invalid', { limit, expected });
}

export function SchemaArrayFieldTemplate({
  canAdd,
  disabled,
  fieldPathId,
  items,
  onAddClick,
  readonly,
  required,
  title,
}: ArrayFieldTemplateProps) {
  const { t } = useI18n();
  return <fieldset className="schema-array" id={fieldPathId.$id}>
    {title && <legend>{title}{required ? ' *' : ''}</legend>}
    <div className="schema-array-items">{items}</div>
    {canAdd && <button className="secondary schema-array-add" type="button" onClick={onAddClick} disabled={disabled || readonly}>＋ {t('common.addItem')}</button>}
  </fieldset>;
}

export function SchemaArrayItemTemplate({
  buttonsProps,
  children,
  hasToolbar,
}: ArrayFieldItemTemplateProps) {
  const { t } = useI18n();
  return <div className="schema-array-item">
    <div className="schema-array-control">{children}</div>
    {hasToolbar && <div className="schema-array-actions">
      {buttonsProps.hasMoveUp && <button type="button" className="secondary compact-button" onClick={buttonsProps.onMoveUpItem} disabled={buttonsProps.disabled || buttonsProps.readonly} aria-label={t('common.moveUp')}>↑</button>}
      {buttonsProps.hasMoveDown && <button type="button" className="secondary compact-button" onClick={buttonsProps.onMoveDownItem} disabled={buttonsProps.disabled || buttonsProps.readonly} aria-label={t('common.moveDown')}>↓</button>}
      {buttonsProps.hasCopy && <button type="button" className="secondary compact-button" onClick={buttonsProps.onCopyItem} disabled={buttonsProps.disabled || buttonsProps.readonly}>{t('common.copy')}</button>}
      {buttonsProps.hasRemove && <button type="button" className="danger compact-button" onClick={buttonsProps.onRemoveItem} disabled={buttonsProps.disabled || buttonsProps.readonly}>{t('common.remove')}</button>}
    </div>}
  </div>;
}

export function SchemaErrorListTemplate({ errors }: ErrorListProps) {
  const { t } = useI18n();
  if (!errors.length) return null;
  return <div className="schema-errors" role="alert">
    <b>{t('schemaError.summary', { count: errors.length })}</b>
    <ul>{errors.map((error, index) => <li key={`${error.property}-${error.name}-${index}`}><code>{error.property?.replace(/^\./, '') || t('schemaError.form')}</code><span>{validationMessage(error, t)}</span></li>)}</ul>
  </div>;
}

export function SchemaFieldErrorTemplate({ errors, fieldPathId }: FieldErrorProps) {
  const { t } = useI18n();
  if (!errors?.length) return null;
  return <div className="schema-field-error" id={`${fieldPathId.$id}__error`} role="status">{t('schemaError.invalidField')}</div>;
}

export function SchemaJsonObjectField({
  autofocus,
  disabled,
  fieldPathId,
  formData,
  onBlur,
  onChange,
  onFocus,
  readonly,
  schema,
}: FieldProps<Record<string, unknown>>) {
  const { t } = useI18n();
  const formatted = JSON.stringify(formData ?? {}, null, 2);
  const [text, setText] = useState(formatted);
  const [error, setError] = useState('');
  useEffect(() => { setText(formatted); }, [formatted]);
  const update = (value: string) => {
    setText(value);
    try {
      const parsed: unknown = JSON.parse(value);
      if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) throw new Error('object required');
      setError('');
      onChange(parsed as Record<string, unknown>, fieldPathId.path, undefined, fieldPathId.$id);
    } catch {
      setError(t('schemaError.jsonObject'));
      onChange(undefined, fieldPathId.path, undefined, fieldPathId.$id);
    }
  };
  return <div className="schema-json-object">
    <textarea id={fieldPathId.$id} aria-label={typeof schema.title === 'string' ? schema.title : t('schemaError.jsonDocument')} rows={12} spellCheck={false} autoFocus={autofocus} disabled={disabled} readOnly={readonly} value={text} onChange={(event) => update(event.target.value)} onBlur={() => onBlur(fieldPathId.$id, text)} onFocus={() => onFocus(fieldPathId.$id, text)} />
    {error && <div className="schema-field-error" role="alert">{error}</div>}
  </div>;
}

export const schemaFormTemplates = {
  ArrayFieldTemplate: SchemaArrayFieldTemplate,
  ArrayFieldItemTemplate: SchemaArrayItemTemplate,
  ErrorListTemplate: SchemaErrorListTemplate,
  FieldErrorTemplate: SchemaFieldErrorTemplate,
};

export const schemaFormFields = {
  JsonObject: SchemaJsonObjectField,
};
