import { useEffect, useRef, useState, useSyncExternalStore } from 'react';
import { api } from '../../api.js';
import { DrawerFrame, RequestDiagnostics } from '../../components.js';
import { Button, Disclosure, Spinner } from '../../design-system/index.js';
import { useI18n } from '../../i18n.js';
import type { RequestDetail, RequestView } from '../../types.js';
import { LatestRequestGate, SessionMonitor, type SessionFocus } from '../SessionMonitor.js';
import { messageOf, queryForTenant } from '../scope/operatorShared.js';
import {
  beginRequestDetailSelection, emptyRequestDetailSelection, rejectRequestDetailSelection,
  requestDetailSelectionInScope, resolveRequestDetailSelection,
} from '../requestDetailSelection.js';
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
  const [requestSelection, setRequestSelection] = useState(emptyRequestDetailSelection);
  const detailRequests = useRef(new LatestRequestGate());

  useEffect(() => {
    detailRequests.current.invalidate();
    setRequestSelection(emptyRequestDetailSelection());
    return () => detailRequests.current.invalidate();
  }, [token, tenant]);

  async function selectRequest(request: RequestView) {
    const credential = token.trim();
    if (!credential) {
      detailRequests.current.invalidate();
      setRequestSelection(emptyRequestDetailSelection());
      return;
    }
    const pending = detailRequests.current.begin();
    setRequestSelection(beginRequestDetailSelection(request, scopeKey));
    try {
      const next = await api<RequestDetail>(
        `/internal/v1/requests/${request.request_id}${queryForTenant(tenant)}`,
        credential,
        { signal: pending.signal },
      );
      if (pending.isCurrent()) {
        setRequestSelection((selection) => resolveRequestDetailSelection(selection, scopeKey, request.request_id, next));
      }
    } catch (reason) {
      if (pending.isCurrent()) {
        setRequestSelection((selection) => rejectRequestDetailSelection(selection, scopeKey, request.request_id, messageOf(reason, t('traffic.detailFailed'))));
      }
    }
  }

  const scopedSelection = requestDetailSelectionInScope(requestSelection, scopeKey);
  const closeRequestDetail = () => {
    detailRequests.current.invalidate();
    setRequestSelection(emptyRequestDetailSelection());
  };
  const retryRequestDetail = () => {
    if (scopedSelection.request) void selectRequest(scopedSelection.request);
  };

  return <>
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
    {scopedSelection.request && <DrawerFrame title={scopedSelection.detail?.model ?? scopedSelection.request.model} eyebrow={t('request.operatorDiagnosis')} onClose={closeRequestDetail}>
      {scopedSelection.phase === 'loading' && <>
        <RequestDiagnostics request={scopedSelection.request} />
        <div className="empty" role="status" aria-live="polite"><Spinner size="extra-small" aria-hidden="true" />{t('common.loading')}</div>
      </>}
      {scopedSelection.phase === 'failed' && <>
        <RequestDiagnostics request={scopedSelection.request} />
        <div className="notice error" role="alert"><span>{scopedSelection.error}</span><Button appearance="secondary" type="button" onClick={retryRequestDetail}>{t('common.retry')}</Button></div>
      </>}
      {scopedSelection.detail && <>
          <RequestDiagnostics request={scopedSelection.detail} />
          <div className="request-diagnostics request-detail-surface request-archive-diagnostics">
            <span><b>{t('self.archive')}</b>{scopedSelection.detail.archive_complete ? t('request.archiveComplete') : t('request.archiveIncomplete')}</span>
            {scopedSelection.detail.provenance && <span><b>{t('request.provenance')}</b>{scopedSelection.detail.provenance.unlinked ? t('request.archiveOnly') : t('request.exactArchive')} · {scopedSelection.detail.provenance.source}</span>}
          </div>
          <Disclosure title={t('request.technicalDetails')}>
            <h3>{t('request.request')}</h3><pre>{JSON.stringify(scopedSelection.detail.request_body, null, 2)}</pre>
            <h3>{t('request.response')}</h3><pre>{JSON.stringify(scopedSelection.detail.response_body, null, 2)}</pre>
            {scopedSelection.detail.provenance && <><h3>{t('request.provenance')}</h3><pre>{JSON.stringify(scopedSelection.detail.provenance, null, 2)}</pre></>}
          </Disclosure>
        </>}
    </DrawerFrame>}
  </>;
}
