import { useEffect, useRef, useState } from 'react';
import { ApiError, api } from '../api';
import { Button, Input } from '../design-system';
import { CopyButton } from '../CopyButton';
import { useI18n } from '../i18n';
import type { KeyView } from '../types';
import { messageOf } from './scope/operatorShared';

const requestPolicy = { cache: 'no-store', credentials: 'omit', referrerPolicy: 'no-referrer' } as const;
type CredentialValue = { key_id: string; credential_generation: number; key: string };

/** An authorized editor reads the actual value; it never substitutes a mask. */
export function ClientCredentialValueEditor({ value, token, tenant, disabled, onSaved }: {
  value: KeyView; token: string; tenant: string; disabled: boolean; onSaved: () => Promise<void>;
}) {
  const { t } = useI18n();
  const [draft, setDraft] = useState('');
  const [baseline, setBaseline] = useState('');
  const [generation, setGeneration] = useState(value.credential_generation);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [saved, setSaved] = useState(false);
  const [retry, setRetry] = useState(0);
  const scope = `${token}\0${tenant}\0${value.key_id}\0${value.credential_generation}\0${value.status ?? 'active'}\0${value.credential_copy_available}\0${disabled}`;
  const identity = useRef({ scope });
  if (identity.current.scope !== scope) identity.current = { scope };
  const currentIdentity = identity.current;
  const [loadedScope, setLoadedScope] = useState('');
  const request = useRef<AbortController | undefined>(undefined);
  const saving = useRef<symbol | undefined>(undefined);
  const active = () => identity.current === currentIdentity;
  const available = (value.status ?? 'active') === 'active' && !disabled;

  useEffect(() => {
    const controller = new AbortController();
    request.current = controller;
    let live = true;
    const current = () => live && identity.current === currentIdentity;
    setDraft(''); setBaseline(''); setError(''); setSaved(false); setGeneration(value.credential_generation); setLoadedScope('');
    if (!available) { setBusy(false); return () => { live = false; controller.abort(); }; }
    if (!value.credential_copy_available) {
      setLoadedScope(scope); setBusy(false);
      return () => { live = false; controller.abort(); };
    }
    setBusy(true);
    void api<CredentialValue>(`/internal/v1/keys/${encodeURIComponent(value.key_id)}/copy`, token, { ...requestPolicy, method: 'POST', signal: controller.signal })
      .then(result => {
        if (!current()) return;
        if (result.key_id !== value.key_id || result.credential_generation !== value.credential_generation) throw new Error(t('credentials.valueChanged'));
        setDraft(result.key); setBaseline(result.key); setLoadedScope(scope);
      })
      .catch(reason => { if (current() && !controller.signal.aborted) setError(reason instanceof ApiError && reason.status === 403 ? t('credentials.copyPermission') : messageOf(reason, t('common.requestFailed'))); })
      .finally(() => { if (current()) setBusy(false); });
    return () => { live = false; controller.abort(); };
  }, [scope, retry]);

  async function save() {
    if (!available || busy || saving.current || loadedScope !== scope || !draft || (draft === baseline && baseline)) return;
    const operation = Symbol('credential-value-save');
    saving.current = operation;
    const controller = new AbortController();
    request.current?.abort(); request.current = controller;
    setBusy(true); setError(''); setSaved(false);
    try {
      const updated = await api<CredentialValue>(`/internal/v1/keys/${encodeURIComponent(value.key_id)}/credential`, token, {
        ...requestPolicy, method: 'PUT', signal: controller.signal,
        body: JSON.stringify({ key: draft, expected_credential_generation: generation }),
      });
      if (!active() || controller.signal.aborted) return;
      if (updated.key_id !== value.key_id || ![generation, generation + 1].includes(updated.credential_generation) || updated.key !== draft) throw new Error(t('credentials.valueChanged'));
      const readback = await api<CredentialValue>(`/internal/v1/keys/${encodeURIComponent(value.key_id)}/copy`, token, { ...requestPolicy, method: 'POST', signal: controller.signal });
      if (!active() || controller.signal.aborted) return;
      if (readback.key_id !== value.key_id || readback.credential_generation !== updated.credential_generation || readback.key !== draft) throw new Error(t('credentials.valueChanged'));
      setGeneration(readback.credential_generation); setDraft(readback.key); setBaseline(readback.key); setSaved(true);
      await onSaved();
    } catch (reason) {
      if (active() && !controller.signal.aborted) setError(messageOf(reason, t('common.requestFailed')));
    } finally {
      if (saving.current === operation) saving.current = undefined;
      if (active() && !controller.signal.aborted) setBusy(false);
    }
  }

  useEffect(() => () => { request.current?.abort(); }, []);
  const visible = loadedScope === scope;
  return <section className="credential-edit-section" aria-label={t('credentials.valueTitle')}>
    <h4>{t('credentials.valueTitle')}</h4>
    <label>{t('credentials.revealedValue')}<Input type="text" autoComplete="off" spellCheck={false} value={visible ? draft : ''} disabled={!available || busy || !visible} onChange={event => { setDraft(event.target.value); setSaved(false); }} /></label>
    {busy && <p role="status">{t('common.loading')}</p>}
    {error && <div role="alert">{error}<Button type="button" appearance="secondary" disabled={busy || !available} onClick={() => setRetry(value => value + 1)}>{t('common.retry')}</Button></div>}
    {saved && <p role="status">{t('credentials.valueSaved')}</p>}
    <div className="row-actions">
      <Button type="button" appearance="primary" disabled={!available || busy || !visible || !draft || draft === baseline} onClick={() => void save()}>{t('credentials.saveValue')}</Button>
      {visible && baseline && <CopyButton value={baseline} fluent />}
    </div>
  </section>;
}
