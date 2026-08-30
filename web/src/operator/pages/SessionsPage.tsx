import { useEffect, useRef, useState, type RefObject } from 'react';
import { api } from '../../api.js';
import { DrawerFrame } from '../../components.js';
import { useI18n } from '../../i18n.js';
import type { RequestDetail, RequestView } from '../../types.js';
import { LatestRequestGate, SessionMonitor, type SessionFocus } from '../SessionMonitor.js';
import { messageOf, queryForTenant } from '../scope/operatorShared.js';
import type { SessionStreamState } from '../SessionMonitor.js';

export function SessionsPage({ token, tenant, focus, revision, eventKeyIds, streamState, streamError, onOpenRequests }: {
  token: string;
  tenant: string;
  focus?: SessionFocus;
  revision: number;
  eventKeyIds: RefObject<Set<string>>;
  streamState: SessionStreamState;
  streamError: string;
  onOpenRequests: () => void;
}) {
  const { t } = useI18n();
  const scopeKey = `${tenant}\0${token}`;
  const [detail, setDetail] = useState<RequestDetail>();
  const [detailScope, setDetailScope] = useState('');
  const [error, setError] = useState('');
  const [errorScope, setErrorScope] = useState('');
  const detailRequests = useRef(new LatestRequestGate());

  useEffect(() => {
    detailRequests.current.invalidate();
    setDetail(undefined);
    setDetailScope('');
    setError('');
    setErrorScope('');
    return () => detailRequests.current.invalidate();
  }, [token, tenant]);

  async function selectRequest(request: RequestView) {
    const credential = token.trim();
    if (!credential || !tenant) {
      detailRequests.current.invalidate();
      setDetail(undefined);
      setDetailScope('');
      setError('');
      setErrorScope('');
      return;
    }
    const pending = detailRequests.current.begin();
    try {
      setError('');
      setErrorScope('');
      const next = await api<RequestDetail>(
        `/internal/v1/requests/${request.request_id}${queryForTenant(tenant)}`,
        credential,
        { signal: pending.signal },
      );
      if (pending.isCurrent()) {
        setDetail(next);
        setDetailScope(scopeKey);
      }
    } catch (reason) {
      if (pending.isCurrent()) {
        setError(messageOf(reason, t('traffic.detailFailed')));
        setErrorScope(scopeKey);
      }
    }
  }

  const scopedDetail = detailScope === scopeKey ? detail : undefined;
  const scopedError = errorScope === scopeKey ? error : '';

  return <>
    {scopedError && <div className="notice error" role="alert">{scopedError}</div>}
    {streamError && <div className="notice error" role="alert">{streamError}</div>}
    <article className="panel">
      <div className="panel-title traffic-heading"><div><h2>{t('sessions.recent')}</h2><span>{t('sessions.monitorHint')}</span></div><div className="segmented" role="group" aria-label={t('sessions.monitorMode')}><button type="button" aria-pressed="false" onClick={onOpenRequests}>{t('sessions.requestsMode')}</button><button type="button" className="active" aria-pressed="true">{t('sessions.sessionsMode')}</button></div></div>
      <SessionMonitor
        token={token}
        tenant={tenant}
        revision={revision}
        eventKeyIds={eventKeyIds}
        focus={focus}
        streamState={streamState}
        onSelectRequest={selectRequest}
      />
    </article>
    {scopedDetail && <DrawerFrame title={scopedDetail.model} eyebrow={t('request.operatorDiagnosis')} onClose={() => { setDetail(undefined); setDetailScope(''); }}><p className="muted break-anywhere">{scopedDetail.request_id} · {scopedDetail.status_code ?? t('common.running')} · {scopedDetail.archive_complete ? t('request.archiveComplete') : t('request.archiveIncomplete')}</p><h3>{t('request.error')}</h3><pre>{scopedDetail.error_code ?? t('common.none')}</pre><h3>{t('request.request')}</h3><pre>{JSON.stringify(scopedDetail.request_body, null, 2)}</pre><h3>{t('request.response')}</h3><pre>{JSON.stringify(scopedDetail.response_body, null, 2)}</pre></DrawerFrame>}
  </>;
}
