import { useEffect, useRef, useState } from 'react';
import { api, apiRead, ApiError } from '../api.js';
import { Button } from '../design-system';
import { useI18n } from '../i18n.js';

interface TitleState {
  title: string | null;
  state: 'unavailable' | 'pending' | 'running' | 'ready' | 'failed' | 'waiting_config';
  error_code: string | null;
}

/** Mount with a credential/session key so old requests cannot change a new selection. */
export function SessionTitleAction({ token, tenant, keyId, sessionId, onTitle }: {
  token: string; tenant: string; keyId: string; sessionId: string;
  onTitle: (title: string) => void;
}) {
  const { t } = useI18n();
  const [status, setStatus] = useState<TitleState>();
  const [sending, setSending] = useState(false);
  const [error, setError] = useState('');
  const [revision, setRevision] = useState(0);
  const callback = useRef(onTitle);
  callback.current = onTitle;
  const params = new URLSearchParams({ key_id: keyId });
  if (tenant) params.set('tenant_external_id', tenant);
  const path = `/internal/v1/sessions/${encodeURIComponent(sessionId)}/title?${params}`;
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
        const result = await apiRead<TitleState>(path, token, { signal: controller.signal, attempts: 1 });
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
      const result = await api<TitleState>(path, token, { method: 'POST', signal: AbortSignal.any([controller.signal, AbortSignal.timeout(15_000)]) });
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
  return <div className="session-title-action">
    <Button appearance="secondary" disabled={busy} onClick={() => { void generate(); }}
      title={t('sessions.title.billing')}>
      {busy ? t('sessions.title.generating') : t('sessions.title.generate')}
    </Button>
    <small className="mtc-secondary-text">{t('sessions.title.billing')}</small>
    {status?.state === 'waiting_config' && <p role="status">{t('sessions.title.configuration')}</p>}
    {(error || status?.state === 'failed') && <p role="status">{error || t('sessions.title.failed')}</p>}
  </div>;
}
