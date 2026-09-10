import RjsfForm, { type FormProps } from '@rjsf/core/lib/components/Form.js';
import type { ObjectFieldTemplateProps, WidgetProps } from '@rjsf/utils';
import { useEffect, useId, useRef, useState, type ReactNode } from 'react';
import { useI18n } from '../i18n';
import { MultiCombobox } from './MultiCombobox';
import './managementForm.css';

export function FormSection({ title, hint, children }: { title: string; hint?: string; children: ReactNode }) {
  const id = useId();
  return <fieldset className="management-form-section" aria-describedby={hint ? id : undefined}>
    <legend>{title}</legend>
    {hint && <p className="field-hint" id={id}>{hint}</p>}
    <div className="management-form-fields">{children}</div>
  </fieldset>;
}

/** A unique ID namespace and a synchronous lock also protect fast Enter/double clicks. */
export function ManagementForm(props: FormProps) {
  const { t } = useI18n();
  const id = useId();
  const locked = useRef(false);
  const mounted = useRef(true);
  const [saving, setSaving] = useState(false);
  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; };
  }, []);
  return <fieldset className="management-schema-form" aria-busy={saving} disabled={props.disabled || saving}>
    <RjsfForm {...props} idPrefix={`management-${id}`} noHtml5Validate focusOnFirstError
      disabled={props.disabled || saving}
      onError={props.onError ?? (() => { /* Localized inline errors and first-error focus are provided by RJSF. */ })}
      onSubmit={async (data, event) => {
        if (locked.current || props.disabled) return;
        locked.current = true; setSaving(true);
        try { await props.onSubmit?.(data, event); }
        finally { locked.current = false; if (mounted.current) setSaving(false); }
      }}>
      <fieldset className="management-form-submit" disabled={props.disabled || saving}>{props.children}</fieldset>
    </RjsfForm>
    {saving && <p className="field-hint" role="status">{t('forms.saving')}</p>}
  </fieldset>;
}

export function ManagedGrantField() { return null; }

/** Root-only grouping keeps schema validation/defaults authoritative, including future fields. */
export function CredentialObjectTemplate(props: ObjectFieldTemplateProps) {
  const { t } = useI18n();
  const groups = [
    { title: 'forms.identity', hint: 'forms.identityHint', names: ['alias', 'principal_external_id', 'tenant_external_id'] },
    { title: 'forms.access', hint: 'credentials.createRoutingHint', names: ['route_ids', 'route_group_ids'] },
    { title: 'forms.balance', hint: 'forms.balanceHint', names: ['currency', 'initial_balance'] },
    { title: 'forms.policy', hint: 'forms.policyHint', names: ['policy'] },
  ];
  const known = new Set(groups.flatMap(group => group.names));
  const context = props.registry.formContext as { routeFields?: ReactNode } | undefined;
  return <div className="credential-form-sections">
    {groups.map(group => {
      const fields = props.properties.filter(property => group.names.includes(property.name));
      const access = group.title === 'forms.access';
      if (!fields.length && !(access && context?.routeFields)) return null;
      return <FormSection key={group.title} title={t(group.title)} hint={t(group.hint)}>
        {access && context?.routeFields}
        {fields.map(property => property.content)}
      </FormSection>;
    })}
    {props.properties.filter(property => !known.has(property.name)).map(property => property.content)}
  </div>;
}

export function ScopePickerWidget({ id, label, value, options, disabled, readonly, required, rawErrors, onChange }: WidgetProps) {
  const { t } = useI18n();
  const choices = (options.enumOptions ?? []).map(option => ({ value: String(option.value), label: option.label }));
  const selected = Array.isArray(value) ? value as string[] : [];
  return <MultiCombobox inputId={id} label={label} options={choices} required={required} invalid={Boolean(rawErrors?.length)}
    value={selected.map(scope => choices.find(choice => choice.value === scope) ?? { value: scope, label: scope })}
    onChange={next => onChange(next.map(choice => choice.value))}
    disabled={disabled || readonly} placeholder={t('forms.searchScopes')} emptyText={t('groups.noMatches')}
    removeLabel={name => t('groups.removeMember', { name })} hint={t('forms.scopesHint')} />;
}
