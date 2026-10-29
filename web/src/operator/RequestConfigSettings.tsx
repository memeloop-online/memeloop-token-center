import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Button, Checkbox, Input } from '@fluentui/react-components';
import { ApiError } from '../api';
import { DetailTooltip, Disclosure, LoadingState } from '../design-system';
import { useI18n } from '../i18n';
import {
  formatConfigValue, parseConfigInput, requestConfigFields, requestConfigPurposes,
  toDisplayValue, type HttpOverrides, type RequestConfigFieldMeta, type RequestConfigPurpose, type RequestConfigSnapshot,
} from './requestConfig';
import { loadRequestConfig, saveRequestConfig, type RequestConfigScope } from './requestConfigApi';
import { messageOf } from './scope/operatorShared';

type Drafts = Record<string, string>;

const defaultScope = 'default';

function scopeDrafts(snapshot: RequestConfigSnapshot, scope: string): Drafts {
  const entries = scope === defaultScope
    ? requestConfigFields.map((field) => [field.key, toDisplayValue(snapshot.settings.defaults[field.key])])
    : requestConfigFields.map((field) => {
      const override = snapshot.settings.purposes[scope as RequestConfigPurpose]?.[field.key];
      return [field.key, toDisplayValue(override)];
    });
  return Object.fromEntries(entries);
}

function effectiveValue(snapshot: RequestConfigSnapshot, purpose: RequestConfigPurpose, field: RequestConfigFieldMeta) {
  return snapshot.effective[purpose][field.key];
}

export function RequestConfigSettings({ token }: { token: string }) {
  const { t } = useI18n();
  const translate = useRef(t);
  translate.current = t;
  const [snapshot, setSnapshot] = useState<RequestConfigSnapshot | null>(null);
  const [drafts, setDrafts] = useState<Record<string, Drafts>>({});
  const [touched, setTouched] = useState<Record<string, boolean>>({});
  const [loading, setLoading] = useState(false);
  const [loadError, setLoadError] = useState('');
  const [unavailable, setUnavailable] = useState(false);
  const [savingScope, setSavingScope] = useState('');
  const [error, setError] = useState('');
  const [message, setMessage] = useState('');
  const [conflict, setConflict] = useState(false);
  const loadSequence = useRef(0);
  const savingRequest = useRef<symbol | null>(null);
  const submittedDrafts = useRef<{ scope: string; drafts: Drafts } | null>(null);
  const units = useMemo(() => ({
    milliseconds: t('settings.requestConfigUnitMillisecond'),
    connections: t('settings.requestConfigUnitConnection'),
  }), [t]);
  const unlimited = t('settings.requestConfigUnlimited');
  const format = useCallback(
    (value: number | null, field: RequestConfigFieldMeta) => formatConfigValue(value, field, unlimited, units),
    [unlimited, units],
  );

  const load = useCallback(async () => {
    const request = ++loadSequence.current;
    savingRequest.current = null; setSavingScope('');
    setError(''); setLoadError(''); setMessage(''); setUnavailable(false); setConflict(false);
    if (!token) { setSnapshot(null); setLoading(false); return; }
    setLoading(true);
    try {
      const next = await loadRequestConfig(token);
      if (request !== loadSequence.current) return;
      setSnapshot(next);
      const nextDrafts: Record<string, Drafts> = { [defaultScope]: scopeDrafts(next, defaultScope) };
      for (const purpose of requestConfigPurposes) nextDrafts[purpose] = scopeDrafts(next, purpose);
      setDrafts(nextDrafts); setTouched({});
    } catch (reason) {
      if (request !== loadSequence.current) return;
      if (reason instanceof ApiError && reason.status === 404) setUnavailable(true);
      else setLoadError(reason instanceof ApiError && (reason.status === 401 || reason.status === 403)
        ? translate.current('settings.requestConfigPermission') : messageOf(reason, translate.current('common.requestFailed')));
    } finally {
      if (request === loadSequence.current) setLoading(false);
    }
  }, [token]);

  useEffect(() => {
    void load();
    return () => { loadSequence.current += 1; };
  }, [load]);

  const rowError = useCallback((field: RequestConfigFieldMeta, text: string | undefined): string => {
    if (text === undefined) return '';
    const parsed = parseConfigInput(text, field);
    if (parsed.ok) return '';
    if (!text.trim()) return translate.current('settings.requestConfigRequired');
    return translate.current(field.nullable ? 'settings.requestConfigInvalidDuration' : 'settings.requestConfigInvalidCount');
  }, []);

  const submit = async (scope: string, values: HttpOverrides, cleared: boolean) => {
    if (savingRequest.current || loading || loadError || conflict || !snapshot) return;
    const operation = Symbol();
    savingRequest.current = operation;
    const request = loadSequence.current;
    submittedDrafts.current = { scope, drafts: { ...drafts[scope] } };
    const current = () => request === loadSequence.current && savingRequest.current === operation;
    const contractScope: RequestConfigScope = scope === defaultScope ? { scope: 'default' } : { scope: 'purpose', purpose: scope as RequestConfigPurpose };
    setSavingScope(scope); setError(''); setMessage('');
    try {
      const next = await saveRequestConfig(token, contractScope, values, snapshot);
      if (!current()) return;
      setSnapshot(next);
      setMessage(translate.current(cleared ? 'settings.requestConfigCleared' : 'settings.requestConfigSaved'));
      setDrafts((previous) => {
        const submitted = submittedDrafts.current;
        if (!submitted || submitted.scope !== scope || JSON.stringify(previous[scope]) !== JSON.stringify(submitted.drafts)) return previous;
        return { ...previous, [scope]: scopeDrafts(next, scope) };
      });
      setTouched((previous) => ({ ...previous, [scope]: false }));
    } catch (reason) {
      if (current()) {
        const stale = reason instanceof ApiError && reason.status === 409;
        setConflict(stale);
        setError(stale ? translate.current('settings.requestConfigConflict')
          : reason instanceof ApiError && (reason.status === 401 || reason.status === 403)
            ? translate.current('settings.requestConfigPermission') : messageOf(reason, translate.current('common.requestFailed')));
      }
    } finally {
      if (savingRequest.current === operation) {
        savingRequest.current = null;
        if (request === loadSequence.current) setSavingScope('');
      }
    }
  };

  const saveScope = (scope: string) => {
    const scopeDraftMap = drafts[scope] ?? {};
    if (requestConfigFields.some((field) => rowError(field, scopeDraftMap[field.key]))) return;
    const values: HttpOverrides = {};
    for (const field of requestConfigFields) {
      const parsed = parseConfigInput(scopeDraftMap[field.key] ?? '', field);
      if (parsed.ok && parsed.value !== undefined) values[field.key] = parsed.value;
    }
    void submit(scope, values, false);
  };

  const clearPurpose = (purpose: RequestConfigPurpose) => {
    void submit(purpose, {}, true);
  };

  const changeDraft = (scope: string, field: string, value: string) => {
    setDrafts((previous) => ({ ...previous, [scope]: { ...previous[scope], [field]: value } }));
    setTouched((previous) => ({ ...previous, [scope]: true }));
    setMessage(''); setError('');
  };

  const renderRow = (scope: string, field: RequestConfigFieldMeta, purpose?: RequestConfigPurpose) => {
    if (!snapshot) return null;
    const inputId = `request-config-${scope}-${field.key}`;
    const draft = drafts[scope]?.[field.key] ?? '';
    const problem = rowError(field, drafts[scope]?.[field.key]);
    const unit = units[field.unit];
    const effective = purpose
      ? effectiveValue(snapshot, purpose, field)
      : { value: snapshot.settings.defaults[field.key] ?? (field.nullable ? null : 8), source: field.key in snapshot.settings.defaults ? 'global_default' as const : 'source_default' as const };
    const inherited = snapshot.settings.defaults[field.key] ?? (field.nullable ? null : 8);
    const placeholder = translate.current('settings.requestConfigInheritsPlaceholder', { value: format(inherited, field) });
    return <div className="request-config-row" key={field.key}>
      <label className="request-config-name" htmlFor={inputId}>
        <DetailTooltip content={t(`settings.requestConfigField.${field.key}Hint`)}><span tabIndex={0}>{t(`settings.requestConfigField.${field.key}`)}</span></DetailTooltip>
      </label>
      <div className="request-config-input">
        <Input id={inputId} inputMode="numeric" value={draft === 'off' ? '' : draft} placeholder={draft === 'off' ? unlimited : placeholder} disabled={Boolean(savingScope) || draft === 'off'}
          aria-invalid={Boolean(problem)} aria-describedby={`${inputId}-state`}
          contentAfter={unit ? <span className="request-config-unit">{unit}</span> : undefined}
          onChange={(_, data) => changeDraft(scope, field.key, data.value)} />
        {field.nullable && <Checkbox label={t('settings.requestConfigDisable')} checked={draft === 'off'} disabled={Boolean(savingScope)}
          onChange={(_, data) => changeDraft(scope, field.key, data.checked ? 'off' : '')} />}
        {problem && <small className="error-text" role="alert">{problem}</small>}
      </div>
      <div className="request-config-state" id={`${inputId}-state`}>
        <span className={`status ${effective.source === 'purpose_override' ? 'pending' : 'ok'}`}>
          {t(effective.source === 'purpose_override' ? 'settings.requestConfigSourceOverride' : effective.source === 'global_default' ? 'settings.requestConfigSourceDefault' : 'settings.requestConfigSourceBuiltin')}
        </span>
        <small>{t('settings.requestConfigEffective', { value: format(effective.value, field) })}</small>
      </div>
    </div>;
  };

  const renderActions = (scope: string, label: string, purpose?: RequestConfigPurpose) => {
    const hasOverride = Boolean(purpose && snapshot && Object.keys(snapshot.settings.purposes[purpose] ?? {}).length > 0);
    const scopeDraftMap = drafts[scope] ?? {};
    const invalid = requestConfigFields.some((field) => rowError(field, scopeDraftMap[field.key]));
    const busy = savingScope === scope;
    return <div className="button-row">
      <Button appearance={purpose ? 'secondary' : 'primary'} disabled={conflict || busy || Boolean(savingScope) || invalid || !touched[scope]} onClick={() => saveScope(scope)}>
        {busy ? t('common.loading') : label}
      </Button>
      {purpose && hasOverride && <Button appearance="subtle" disabled={conflict || Boolean(savingScope)} onClick={() => clearPurpose(purpose)}>
        {t('settings.requestConfigClearOverride')}
      </Button>}
    </div>;
  };

  return <article className="settings-card request-config" aria-busy={loading || Boolean(savingScope)} aria-labelledby="request-config-title">
    <div className="settings-card-heading">
      <div>
        <h3 id="request-config-title">{t('settings.requestConfigTitle')}</h3>
        <p className="muted">{t('settings.requestConfigDescription')}</p>
      </div>
      {snapshot && <span className="status ok">{t('settings.requestConfigRevision', { revision: snapshot.revision })}</span>}
    </div>
    {error && <div className="notice error" role="alert">{error}</div>}
    {conflict && <Button onClick={() => void load()}>{t('settings.requestConfigReload')}</Button>}
    {message && <div className="notice success" role="status">{message}</div>}
    {loading ? <LoadingState label={t('common.loading')} level="page" />
      : loadError ? <div className="settings-empty" role="alert"><b>{t('settings.requestConfigLoadFailed')}</b><span>{loadError}</span><button type="button" className="secondary" onClick={() => void load()}>{t('common.retry')}</button></div>
        : unavailable ? <p className="settings-status-note">{t('settings.requestConfigUnavailable')}</p>
          : !snapshot ? null : <>
            <section className="request-config-scope" aria-label={t('settings.requestConfigDefaults')}>
              <div>
                <h4>{t('settings.requestConfigDefaults')}</h4>
                <p className="muted">{t('settings.requestConfigDefaultsHint')}</p>
              </div>
              {requestConfigFields.map((field) => renderRow(defaultScope, field))}
              {renderActions(defaultScope, t('settings.requestConfigSaveDefaults'))}
            </section>
            <section className="request-config-scope" aria-label={t('settings.requestConfigPurposeOverrides')}>
              <div>
                <h4>{t('settings.requestConfigPurposeOverrides')}</h4>
                <p className="muted">{t('settings.requestConfigPurposeHint')}</p>
              </div>
              {requestConfigPurposes.map((purpose) => <Disclosure key={purpose} title={t(`settings.requestConfigPurpose.${purpose}`)}>
                <div className="request-config-purpose">
                  {requestConfigFields.map((field) => renderRow(purpose, field, purpose))}
                  {renderActions(purpose, t('settings.requestConfigSavePurpose'), purpose)}
                </div>
              </Disclosure>)}
            </section>
          </>}
  </article>;
}
