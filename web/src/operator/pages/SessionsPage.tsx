import { useEffect, useRef, useState, useSyncExternalStore } from 'react';
import { api } from '../../api.js';
import { DrawerFrame, RequestDiagnostics } from '../../components.js';
import { Disclosure, Spinner } from '../../design-system/index.js';
import { useI18n } from '../../i18n.js';
import type { RequestDetail, RequestView } from '../../types.js';
import { LatestRequestGate, SessionMonitor, type SessionFocus } from '../SessionMonitor.js';
import { messageOf, queryForTenant } from '../scope/operatorShared.js';
import type { SessionStreamState } from '../SessionMonitor.js';
import type { SessionEventChannel } from '../sessionEventChannel.js';

export function SessionsPage({ token, tenant, focus, sessionEvents, streamState, streamError, onOpenRequests, requestRefresh }: {
  token: string;
  tenant: string;
  focus?: SessionFocus;
  sessionEvents: SessionEventChannel;
  streamState: SessionStreamState;
  streamError: string;
  onOpenRequests: () => void;
  requestRefresh?: { intervalMs: number; paused: boolean; onIntervalChange: (value: number) => void };
}) {
  const revision = useSyncExternalStore(sessionEvents.subscribe, sessionEvents.snapshot);
  const { t } = useI18n();
  const scopeKey = `${tenant}\0${token}`;
  const [detail, setDetail] = useState<RequestDetail>();
  const [detailScope, setDetailScope] = useState('');
  const [selectedRequest, setSelectedRequest] = useState<RequestView>();
  const [selectedRequestScope, setSelectedRequestScope] = useState('');
  const [requestLoading, setRequestLoading] = useState(false);
  const [error, setError] = useState('');
  const [errorScope, setErrorScope] = useState('');
  const detailRequests = useRef(new LatestRequestGate());

  useEffect(() => {
    detailRequests.current.invalidate();
    setDetail(undefined);
    setDetailScope('');
    setSelectedRequest(undefined);
    setSelectedRequestScope('');
    setRequestLoading(false);
    setError('');
    setErrorScope('');
    return () => detailRequests.current.invalidate();
  }, [token, tenant]);

  async function selectRequest(request: RequestView) {
    const credential = token.trim();
    if (!credential) {
      detailRequests.current.invalidate();
      setDetail(undefined);
      setDetailScope('');
      setSelectedRequest(undefined);
      setSelectedRequestScope('');
      setRequestLoading(false);
      setError('');
      setErrorScope('');
      return;
    }
    const pending = detailRequests.current.begin();
    setDetail(undefined);
    setDetailScope('');
    setSelectedRequest(request);
    setSelectedRequestScope(scopeKey);
    setRequestLoading(true);
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
    } finally {
      if (pending.isCurrent()) setRequestLoading(false);
    }
  }

  const scopedDetail = detailScope === scopeKey ? detail : undefined;
  const scopedSelectedRequest = selectedRequestScope === scopeKey ? selectedRequest : undefined;
  const scopedError = errorScope === scopeKey ? error : '';
  const closeRequestDetail = () => {
    detailRequests.current.invalidate();
    setDetail(undefined);
    setDetailScope('');
    setSelectedRequest(undefined);
    setSelectedRequestScope('');
    setRequestLoading(false);
  };

  return <>
    {scopedError && <div className="notice error" role="alert">{scopedError}</div>}
    {streamError && <div className="notice error" role="alert">{streamError}</div>}
    <article className="panel sessions-page">
      <div className="panel-title traffic-heading"><div><h2>{t('sessions.recent')}</h2><span>{t('sessions.monitorHint')}</span></div><div className="segmented" role="group" aria-label={t('sessions.monitorMode')}><button type="button" aria-pressed="false" onClick={onOpenRequests}>{t('sessions.requestsMode')}</button><button type="button" className="active" aria-pressed="true">{t('sessions.sessionsMode')}</button></div></div>
      <SessionMonitor
        token={token}
        tenant={tenant}
        revision={revision}
        eventKeyIds={sessionEvents.eventKeyIds}
        eventOverflowed={sessionEvents.overflowed}
        focus={focus}
        streamState={streamState}
        refreshCadence={requestRefresh}
        onSelectRequest={selectRequest}
      />
    </article>
    {(scopedDetail || (requestLoading && scopedSelectedRequest)) && <DrawerFrame title={scopedDetail?.model ?? scopedSelectedRequest!.model} eyebrow={t('request.operatorDiagnosis')} onClose={closeRequestDetail}>
      {requestLoading && !scopedDetail
        ? <div className="empty" role="status" aria-live="polite"><Spinner size="extra-small" aria-hidden="true" />{t('common.loading')}</div>
        : scopedDetail && <>
          <RequestDiagnostics request={scopedDetail} />
          <div className="request-diagnostics request-detail-surface request-archive-diagnostics">
            <span><b>{t('self.archive')}</b>{scopedDetail.archive_complete ? t('request.archiveComplete') : t('request.archiveIncomplete')}</span>
            {scopedDetail.provenance && <span><b>{t('request.provenance')}</b>{scopedDetail.provenance.unlinked ? t('request.archiveOnly') : t('request.exactArchive')} · {scopedDetail.provenance.source}</span>}
          </div>
          <Disclosure title={t('request.technicalDetails')}>
            <h3>{t('request.request')}</h3><pre>{JSON.stringify(scopedDetail.request_body, null, 2)}</pre>
            <h3>{t('request.response')}</h3><pre>{JSON.stringify(scopedDetail.response_body, null, 2)}</pre>
            {scopedDetail.provenance && <><h3>{t('request.provenance')}</h3><pre>{JSON.stringify(scopedDetail.provenance, null, 2)}</pre></>}
          </Disclosure>
        </>}
    </DrawerFrame>}
  </>;
}
