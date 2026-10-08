import type { WidgetProps } from '@rjsf/utils';
import { Input } from '../design-system';
import { schemaTextValue } from '../schemaTextValue';

/** Schema validation and secret-specific fields remain owned by the form layer. */
function TextWidget({ id, value, schema, options, required, disabled, readonly, onChange, onBlur, onFocus, placeholder, rawErrors, autofocus }: WidgetProps) {
  const text = schemaTextValue(value, schema.default, options.emptyValue);
  return <Input id={id} value={text} required={required} disabled={disabled} readOnly={readonly} placeholder={placeholder}
    autoFocus={autofocus} aria-invalid={Boolean(rawErrors?.length)}
    onChange={(_, data) => onChange(data.value)} onBlur={() => onBlur(id, text)} onFocus={() => onFocus(id, text)} />;
}

// PasswordWidget is deliberately not overridden: PR91 owns secret rendering.
export const fluentFormWidgets = { TextWidget };
