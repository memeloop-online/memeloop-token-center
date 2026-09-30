import RjsfForm from '@rjsf/core/lib/components/Form.js';
import { ariaDescribedByIds, type RJSFSchema, type WidgetProps } from '@rjsf/utils';
import { useEffect, useId, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { ApiError, api } from '../api';
import { Button, Checkbox, FormSection, Input, Select } from '../design-system';
import { useI18n } from '../i18n';
import { schemaFormTemplates } from '../SchemaTemplates';
import { SecureSchemaField } from '../SecureSchemaField';
import { prepareSecretForm } from '../secretSchema';
import { safeValidator } from '../safeValidator';
import type { ProviderType, UpstreamAccount } from '../types';
import { upstreamRotationCopy } from './upstreamRotationCopy';
import { upstreamRotationSchema, upstreamRotationUiSchema } from './upstreamRotationSchema';
import './upstreamCredentialRotation.css';

function RotationSelect({ id, value, options, required, disabled, readonly, onChange, onBlur, onFocus, rawErrors }: WidgetProps) {
  const { locale } = useI18n();
  const copy = upstreamRotationCopy(locale);
  return <Select id={id} value={value == null ? '' : String(value)} required={required} disabled={disabled || readonly}
    aria-describedby={ariaDescribedByIds(id)} aria-invalid={Boolean(rawErrors?.length)}
    onChange={(_, data) => onChange(options.enumOptions?.find(option => String(option.value) === data.value)?.value)}
    onBlur={() => onBlur(id, value)} onFocus={() => onFocus(id, value)}>
    <option value="">{copy.select}</option>
    {options.enumOptions?.map(option => <option key={String(option.value)} value={String(option.value)}>{copy.types[option.value as keyof typeof copy.types] ?? option.label}</option>)}
  </Select>;
}

function RotationText({ id, value, required, disabled, readonly, onChange, onBlur, onFocus, rawErrors }: WidgetProps) {
  return <Input id={id} value={value == null ? '' : String(value)} required={required} disabled={disabled} readOnly={readonly}
    aria-describedby={ariaDescribedByIds(id)} aria-invalid={Boolean(rawErrors?.length)}
    onChange={(_, data) => onChange(data.value || undefined)} onBlur={() => onBlur(id, value)} onFocus={() => onFocus(id, value)} />;
}

function RotationNumber({ id, value, required, disabled, readonly, onChange, onBlur, onFocus, rawErrors }: WidgetProps) {
  return <Input id={id} type="number" value={value == null ? '' : String(value)} required={required} disabled={disabled} readOnly={readonly}
    aria-describedby={ariaDescribedByIds(id)} aria-invalid={Boolean(rawErrors?.length)}
    onChange={(_, data) => {
      if (data.value === '') { onChange(undefined); return; }
      const parsed = Number(data.value);
      onChange(Number.isNaN(parsed) ? undefined : parsed);
    }} onBlur={() => onBlur(id, value)} onFocus={() => onFocus(id, value)} />;
}

function RotationField(props: WidgetProps) {
  const types = Array.isArray(props.schema.type) ? props.schema.type : [props.schema.type];
  return types.includes('integer') || types.includes('number') ? <RotationNumber {...props} /> : <RotationText {...props} />;
}

export function UpstreamCredentialRotation({ account, provider, token, allowed, onBack, onSaved }: {
  account: UpstreamAccount; provider: ProviderType; token: string; allowed: boolean;
  onBack: () => void; onSaved: (account: UpstreamAccount) => void;
}) {
  const { locale, t } = useI18n();
  const copy = upstreamRotationCopy(locale);
  const id = useId();
  const title = useRef<HTMLHeadingElement>(null);
  const feedback = useRef<HTMLDivElement>(null);
  const submitting = useRef(false);
  const controller = useRef<AbortController | null>(null);
  const [busy, setBusy] = useState(false);
  const [acknowledged, setAcknowledged] = useState(false);
  const [error, setError] = useState<keyof Pick<typeof copy, 'failed' | 'invalid' | 'forbidden' | 'conflict' | 'required'> | ''>('');
  const prepared = useMemo(() => prepareSecretForm(upstreamRotationSchema(provider.credential_schema as RJSFSchema, locale), safeValidator), [provider.credential_schema, locale]);
  useLayoutEffect(() => { title.current?.focus(); }, []);
  useLayoutEffect(() => { if (error && !busy) feedback.current?.focus(); }, [error, busy]);
  useEffect(() => () => { controller.current?.abort(); }, []);
  const canSubmit = allowed && account.can_rotate && Boolean(token);

  async function submit(credential: unknown) {
    if (!canSubmit || !acknowledged || submitting.current) return;
    submitting.current = true;
    setBusy(true); setError('');
    const request = new AbortController();
    controller.current = request;
    try {
      const updated = await api<UpstreamAccount>(`/internal/v1/upstreams/${account.id}/credential`, token, {
        method: 'PUT', headers: { 'Idempotency-Key': crypto.randomUUID() },
        body: JSON.stringify({ credential }), signal: request.signal,
      });
      if (!request.signal.aborted) onSaved(updated);
    } catch (reason) {
      if (!request.signal.aborted) setError(reason instanceof ApiError
        ? reason.status === 401 || reason.status === 403 ? 'forbidden' : reason.status === 409 ? 'conflict' : reason.status === 400 || reason.status === 422 ? 'invalid' : 'failed'
        : 'failed');
    } finally {
      submitting.current = false;
      if (!request.signal.aborted) setBusy(false);
    }
  }

  return <article className="create-resource create-journey upstream-credential-rotation" data-open="true" aria-labelledby={`${id}-title`} aria-busy={busy}>
    <header className="journey-heading">
      <h2 id={`${id}-title`} ref={title} tabIndex={-1}>{t('providers.rotateFor', { name: account.name })}</h2>
      <Button appearance="subtle" type="button" disabled={busy} onClick={onBack}>{copy.close}</Button>
    </header>
    <p className="create-journey-description">{copy.purpose}</p>
    <div className="create-resource-body form-panel">
      <FormSection title={copy.impactTitle}>
        <p>{copy.impact}</p><p>{copy.oldCredential}</p><p>{copy.permission}</p>
      </FormSection>
      {!canSubmit && <p role="note">{copy.denied}</p>}
      <div ref={feedback} tabIndex={-1} role={error ? 'alert' : undefined}>{error && copy[error]}</div>
      <FormSection title={copy.fields} description={copy.options}>
        <RjsfForm key={locale} idPrefix={`${id}-credential`} schema={prepared.schema} validator={safeValidator}
          formContext={{ fluentSecrets: true }}
          uiSchema={upstreamRotationUiSchema(locale)} fields={{ SchemaField: SecureSchemaField }}
          templates={schemaFormTemplates} widgets={{ TextWidget: RotationField, UpDownWidget: RotationField, SelectWidget: RotationSelect }}
          disabled={!canSubmit || busy} noHtml5Validate showErrorList={false} omitExtraData liveOmit
          experimental_defaultFormStateBehavior={{ emptyObjectFields: 'skipEmptyDefaults' }}
          onError={() => { setError('required'); feedback.current?.focus(); }}
          onSubmit={({ formData }) => void submit(formData)}>
          <Checkbox checked={acknowledged} disabled={!canSubmit || busy} label={copy.acknowledge}
            onChange={(_, data) => setAcknowledged(data.checked === true)} />
          <div className="journey-actions">
            <Button appearance="secondary" type="button" disabled={busy} onClick={onBack}>{copy.back}</Button>
            <Button appearance="primary" type="submit" disabled={!canSubmit || !acknowledged || busy}>{busy ? copy.saving : copy.submit}</Button>
          </div>
        </RjsfForm>
      </FormSection>
    </div>
  </article>;
}
