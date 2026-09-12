import type { FieldProps, RJSFSchema, UiSchema } from '@rjsf/utils';
import { useI18n } from '../i18n';
import './operatorFormSurfaces.css';

type EnforcementMode = 'prepaid' | 'metered_unlimited';
const isEnforcementMode = (value: unknown): value is EnforcementMode => value === 'prepaid' || value === 'metered_unlimited';

// The service expresses this enum as oneOf/const branches without a parent
// type. A dedicated field avoids RJSF's unsupported-schema fallback while
// retaining the original service schema for validation.
function EnforcementModeField({ formData, onChange, fieldPathId, disabled, readonly, required, rawErrors, autofocus, onBlur, onFocus }: FieldProps) {
  const { t } = useI18n();
  const value: unknown = formData ?? 'prepaid';
  const supported = isEnforcementMode(value);
  const hintId = `${fieldPathId.$id}__hint`;
  return <div className="credential-enforcement-field">
    <label htmlFor={fieldPathId.$id}>{t('credentials.enforcementMode')}{required ? ' *' : ''}</label>
    <select id={fieldPathId.$id} value={supported ? value : ''} disabled={disabled || readonly} required={required} autoFocus={autofocus}
      aria-describedby={hintId} aria-invalid={Boolean(rawErrors?.length) || !supported}
      onBlur={() => onBlur(fieldPathId.$id, value)} onFocus={() => onFocus(fieldPathId.$id, value)}
      onChange={(event) => { if (isEnforcementMode(event.target.value)) onChange(event.target.value, fieldPathId.path, undefined, fieldPathId.$id); }}>
      {!supported && <option value="" disabled>{t('credentials.chooseEnforcementMode')}</option>}
      <option value="prepaid">{t('enforcementMode.prepaid')}</option>
      <option value="metered_unlimited">{t('enforcementMode.metered_unlimited')}</option>
    </select>
    <p className="field-description" id={hintId}>{t('credentials.enforcementHint')}</p>
  </div>;
}

export const credentialFormFields = { EnforcementMode: EnforcementModeField };
export const credentialPolicyUiSchema: UiSchema = { enforcement_mode: { 'ui:field': 'EnforcementMode' } };
export const credentialCreateUiSchema: UiSchema = {
  tenant_external_id: { 'ui:widget': 'hidden' },
  policy: credentialPolicyUiSchema,
};

// Routing is owned by the named comboboxes and injected into the request.
// Remove only those root fields; leave policy and future plugin fields intact.
export function credentialCreateSchema(schema: RJSFSchema): RJSFSchema {
  const routingFields = new Set(['route_ids', 'route_group_ids']);
  return {
    ...schema,
    properties: Object.fromEntries(Object.entries(schema.properties ?? {}).filter(([name]) => !routingFields.has(name))),
    required: schema.required?.filter((name) => !routingFields.has(name)),
  };
}
