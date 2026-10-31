import { useEffect, useRef, useState } from 'react';
import { api, apiRead, ApiError } from '../api.js';
import { Button, Input } from '../design-system';
import { useI18n } from '../i18n.js';
import { updateSessionName } from './sessionTasks.js';
import type { SessionTitleState } from '../types.js';

/** Mount with a credential/session key so old requests cannot change a new selection. */
export function SessionTitleAction({ token, tenant, keyId, sessionId, onTitle }: {
  token: string; tenant: string; keyId: string; sessionId: string;
  onTitle: (title: string) => void;
}) {
  const { t } = useI18n();
  const [status, setStatus] = useState<SessionTitleState>();
  const [sending, setSending] = useState(false);
  const [error, setError] = useState('');
  const [revision, setRevision] = useState(0);
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState('');
  const callback = useRef(onTitle);
  callback.current = onTitle;
  const params = new URLSearchParams({ key_id: keyId });
  if (tenant) params.set('tenant_external_id', tenant);
  const path = `/internal/v1/sessions/${encodeURIComponent(sessionId)}/title?${params}`;
  // The naming routes address stable cluster identities only.
  const nameable = !sessionId.startsWith('unlinked:');
  const mounted = useRef(true);
  const mutation = useRef<AbortController | undefined>(undefined);
  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; mutation.current?.abort(); };
  }, []);
  useEffect(() => {
    const controller = new AbortController();
    let timer: ReturnType<typeof setTimeout> | undefined;
    const read = async () => {
      try {
        const result = await apiRead<SessionTitleState>(path, token, { signal: controller.signal, attempts: 1 });
        if (controller.signal.aborted) return;
        setStatus(result);
        if (result.title) callback.current(result.title);
        if (result.state === 'pending' || result.state === 'running') timer = setTimeout(() => { void read(); }, 5000);
      } catch {
        // Title status must never prevent opening the conversation.
        if (!controller.signal.aborted) timer = setTimeout(() => { void read(); }, 30_000);
      }
    };
    void read();
    return () => { controller.abort(); clearTimeout(timer); };
  }, [path, token, revision]);
  const busy = sending || status?.state === 'pending' || status?.state === 'running';
  const generate = async () => {
    if (busy) return;
    setSending(true);
    setError('');
    const controller = new AbortController();
    mutation.current = controller;
    try {
      const result = await api<SessionTitleState>(path, token, { method: 'POST', signal: AbortSignal.any([controller.signal, AbortSignal.timeout(15_000)]) });
      if (!mounted.current || controller.signal.aborted) return;
      setStatus(result);
      setRevision((value) => value + 1);
    } catch (reason) {
      if (mounted.current && !controller.signal.aborted) setError(reason instanceof ApiError && reason.status === 403
        ? t('sessions.title.permissionDenied') : t('sessions.title.failed'));
    } finally {
      if (mounted.current) setSending(false);
    }
  };
  const saveName = async () => {
    const title = draft.trim();
    if (!title || [...title].length > 80 || /[\u0000-\u001f\u007f]/.test(title) || busy || !status) return;
    setSending(true);
    setError('');
    const controller = new AbortController();
    mutation.current = controller;
    try {
      const result = await updateSessionName(tenant, token, { key_id: keyId, session_id: sessionId },
        { title, source: 'manual', expected_revision: status?.revision ?? 0 }, controller.signal);
      if (!mounted.current || controller.signal.aborted) return;
      setStatus(result);
      setEditing(false);
      if (result.title) callback.current(result.title);
    } catch (reason) {
      if (!mounted.current || controller.signal.aborted) return;
      if (reason instanceof ApiError && reason.status === 409) {
        // A concurrent namer won. Re-read so the latest name replaces the draft.
        setError(t('sessions.name.conflict'));
        setEditing(false);
        setRevision((value) => value + 1);
      } else {
        setError(reason instanceof ApiError && reason.status === 403
          ? t('sessions.title.permissionDenied') : t('sessions.name.failed'));
      }
    } finally {
      if (mounted.current) setSending(false);
    }
  };
  const titleSource = status?.source;
  return <div className="session-title-action">
    {editing ? <>
      <Input aria-label={t('sessions.name.placeholder')} value={draft} disabled={sending}
        placeholder={t('sessions.name.placeholder')}
        onChange={(_, data) => setDraft(data.value)}
        onKeyDown={(event) => { if (event.key === 'Enter') { event.preventDefault(); void saveName(); } if (event.key === 'Escape') setEditing(false); }} />
      <Button appearance="primary" disabled={sending || !status || !draft.trim() || [...draft.trim()].length > 80 || /[\u0000-\u001f\u007f]/.test(draft.trim())} onClick={() => { void saveName(); }}>{t('sessions.name.save')}</Button>
      <Button appearance="secondary" disabled={sending} onClick={() => setEditing(false)}>{t('common.cancel')}</Button>
    </> : <>
      <Button appearance="secondary" disabled={busy || titleSource === 'manual'} onClick={() => { void generate(); }}
        title={titleSource === 'manual' ? t('sessions.name.protectedManual') : t('sessions.title.billing')}>
        {busy ? t('sessions.title.generating') : t('sessions.title.generate')}
      </Button>
      {nameable && <Button appearance="secondary" disabled={busy || !status} onClick={() => { setDraft(status?.title ?? ''); setError(''); setEditing(true); }}>{t('sessions.name.manual')}</Button>}
      <small className="mtc-secondary-text">{t('sessions.title.billing')}</small>
      {titleSource === 'manual' && <small className="mtc-secondary-text">{t('sessions.name.source.manual')} · {t('sessions.name.protectedManual')}</small>}
      {titleSource === 'codex' && <small className="mtc-secondary-text">{t('sessions.name.source.codex')}</small>}
    </>}
    {status?.state === 'waiting_config' && <p role="status">{t('sessions.title.configuration')}</p>}
    {(error || status?.state === 'failed') && <p role="status">{error || t('sessions.title.failed')}</p>}
  </div>;
}
