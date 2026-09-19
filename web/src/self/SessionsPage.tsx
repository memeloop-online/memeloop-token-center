import { useCallback, useEffect, useRef, useState } from 'react';
import { api } from '../api';
import { readArchiveRange, type ArchiveRangeLoader } from '../archiveRange';
import { Button } from '../design-system';
import { formatNumber } from '../format';
import { useI18n } from '../i18n';
import { RequestRefreshControl } from '../operator/traffic/RequestRefreshControl';
import { SessionDetailSurface, SessionList } from '../SessionViews';
import type { KeyView, LogicalSessionCursor, LogicalSessionDetail, LogicalSessionListResponse, LogicalSessionSummary, RequestDetail, RequestView } from '../types';
import { selfErrorMessage } from './errors';
import { sessionDetailPath, sessionsPath } from './requestPaths';
import { useSelfRequestRefresh } from './useSelfRequestRefresh';
import './requestFilters.css';

export function SessionsPage({ credential, credentialView, focusSessionId, onError, onOpenRequest }: {
  credential: string;
  credentialView: KeyView;
  focusSessionId?: string;
  onError: (message: string) => void;
  onOpenRequest: (request: RequestView) => void;
}) {
  const { locale, t } = useI18n();
  const [sessions, setSessions] = useState<LogicalSessionSummary[]>([]);
  const [nextCursor, setNextCursor] = useState<LogicalSessionCursor | null>(null);
  const [generatedAt, setGeneratedAt] = useState(0);
  const [selected, setSelected] = useState<LogicalSessionSummary>();
  const [detail, setDetail] = useState<LogicalSessionDetail>();
  const [loading, setLoading] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const listSequence = useRef(0);
  const detailSequence = useRef(0);
  const listController = useRef<AbortController | undefined>(undefined);
  const detailController = useRef<AbortController | undefined>(undefined);
  const listInFlight = useRef(false);
  const historyLoaded = useRef(false);
  const loadReplayArchive = useCallback((request: RequestView, signal: AbortSignal) => api<RequestDetail>(
    `/self/v1/requests/${encodeURIComponent(request.request_id)}`, credential, { signal },
  ), [credential]);
  const loadArchiveRange = useCallback<ArchiveRangeLoader>((requestId, side, offset, length, etag, signal) => readArchiveRange(
    `/self/v1/requests/${encodeURIComponent(requestId)}/archive/${side}`, credential, offset, length, etag, AbortSignal.any([signal, AbortSignal.timeout(15_000)]),
  ), [credential]);

  async function fetchSessions(before?: LogicalSessionCursor, mode: 'replace' | 'append' | 'refresh' = 'replace') {
    const sequence = ++listSequence.current;
    listController.current?.abort();
    const controller = new AbortController();
    listController.current = controller;
    listInFlight.current = true;
    if (mode === 'refresh') setRefreshing(true);
    else { setLoading(true); onError(''); }
    try {
      const response = await api<LogicalSessionListResponse>(sessionsPath(before, before ? undefined : focusSessionId), credential, { signal: controller.signal });
      if (sequence !== listSequence.current || controller.signal.aborted) return;
      if (mode === 'refresh') {
        if (historyLoaded.current) {
          const fresh = new Map(response.sessions.map((session) => [session.session_id, session]));
          setSessions((current) => current.map((session) => fresh.get(session.session_id) ?? session));
          setSelected((current) => current ? fresh.get(current.session_id) ?? current : current);
        } else {
          setSessions(response.sessions);
          setNextCursor(response.next_cursor);
          setSelected((current) => current ? response.sessions.find((session) => session.session_id === current.session_id) ?? current : current);
        }
        setGeneratedAt(response.generated_at);
        onError('');
        return;
      }
      setSessions((current) => {
        if (!before) return response.sessions;
        const known = new Set(current.map((session) => session.session_id));
        return [...current, ...response.sessions.filter((session) => !known.has(session.session_id))];
      });
      setNextCursor(response.next_cursor);
      setGeneratedAt(response.generated_at);
      if (mode === 'append') historyLoaded.current = true;
      else historyLoaded.current = false;
      if (!before && focusSessionId) {
        const focused = response.sessions.find((session) => session.session_id === focusSessionId);
        if (focused) void selectSession(focused);
        else onError(t('self.resourceMissing'));
      } else if (!before && response.sessions[0]) {
        void selectSession(response.sessions[0]);
      }
    } catch (reason) {
      if (sequence === listSequence.current && !controller.signal.aborted) onError(selfErrorMessage(reason, t, t('common.requestFailed')));
    } finally {
      if (listController.current === controller) listInFlight.current = false;
      if (sequence === listSequence.current) {
        setLoading(false);
        setRefreshing(false);
      }
    }
  }

  async function selectSession(session: LogicalSessionSummary) {
    const sequence = ++detailSequence.current;
    detailController.current?.abort();
    const controller = new AbortController();
    detailController.current = controller;
    setSelected(session);
    setLoading(true);
    onError('');
    try {
      const response = await api<LogicalSessionDetail>(sessionDetailPath(session.session_id), credential, { signal: controller.signal });
      if (sequence === detailSequence.current && !controller.signal.aborted) setDetail(response);
    } catch (reason) {
      if (sequence === detailSequence.current && !controller.signal.aborted) onError(selfErrorMessage(reason, t, t('common.requestFailed')));
    } finally {
      if (sequence === detailSequence.current) setLoading(false);
    }
  }

  async function fetchEarlierDetail() {
    if (!selected || !detail?.next_cursor) return;
    const sequence = ++detailSequence.current;
    detailController.current?.abort();
    const controller = new AbortController();
    detailController.current = controller;
    setLoading(true);
    onError('');
    try {
      const page = await api<LogicalSessionDetail>(sessionDetailPath(selected.session_id, detail.next_cursor), credential, { signal: controller.signal });
      if (sequence !== detailSequence.current || controller.signal.aborted) return;
      setDetail((latest) => {
        if (!latest || latest.session_id !== page.session_id) return latest;
        const requestIds = new Set(page.requests.map((request) => request.request_id));
        const edgeKeys = new Set(page.edges.map((edge) => `${edge.from_request_id ?? ''}:${edge.to_request_id}:${edge.relation}`));
        return {
          ...page,
          requests: [...page.requests, ...latest.requests.filter((request) => !requestIds.has(request.request_id))],
          edges: [...page.edges, ...latest.edges.filter((edge) => !edgeKeys.has(`${edge.from_request_id ?? ''}:${edge.to_request_id}:${edge.relation}`))],
          edges_truncated: page.edges_truncated || latest.edges_truncated,
        };
      });
    } catch (reason) {
      if (sequence === detailSequence.current && !controller.signal.aborted) onError(selfErrorMessage(reason, t, t('common.requestFailed')));
    } finally {
      if (sequence === detailSequence.current) setLoading(false);
    }
  }

  useEffect(() => {
    listSequence.current += 1;
    detailSequence.current += 1;
    listController.current?.abort();
    detailController.current?.abort();
    setSessions([]);
    setNextCursor(null);
    setGeneratedAt(0);
    setSelected(undefined);
    setDetail(undefined);
    setRefreshing(false);
    historyLoaded.current = false;
    listInFlight.current = false;
    setLoading(true);
    void fetchSessions();
    return () => {
      listSequence.current += 1;
      detailSequence.current += 1;
      listController.current?.abort();
      detailController.current?.abort();
    };
  }, [credential, focusSessionId]);

  const { intervalMs, paused, setIntervalMs } = useSelfRequestRefresh(() => {
    if (listInFlight.current || document.hidden) return;
    void fetchSessions(undefined, 'refresh');
  }, generatedAt > 0);

  return <div className="self-page self-sessions-page" data-self-page="sessions">
    <article className="panel self-sessions">
      <div className="panel-title self-session-header">
        <div><h2>{t('sessions.selfTitle')}</h2><span>{t('sessions.loaded', { count: formatNumber(sessions.length, locale) })}{generatedAt > 0 && ` · ${t('sessions.generatedAt', { time: new Date(generatedAt).toLocaleString(locale) })}`}</span></div>
        <div className="self-request-refresh">
          <RequestRefreshControl intervalMs={intervalMs} onIntervalChange={setIntervalMs} paused={paused} supportsLive={false} refreshing={refreshing} />
          <Button appearance="secondary" type="button" disabled={loading || refreshing} onClick={() => void fetchSessions(undefined, 'refresh')}>{t('sessions.refreshNow')}</Button>
        </div>
      </div>
      <div className="session-workspace self-session-workspace">
        <section className="session-browser" aria-label={t('sessions.recent')}>
          <SessionList values={sessions} loading={loading} showCredential={false} selected={selected} layout="sidebar" onSelect={(session) => void selectSession(session)} />
          {nextCursor && <div className="load-more"><button type="button" className="secondary" disabled={loading} onClick={() => void fetchSessions(nextCursor, 'append')}>{loading ? t('common.loading') : t('sessions.loadOlder')}</button></div>}
        </section>
        <div className="session-detail-region">
          {detail && <SessionDetailSurface detail={detail} summary={selected} currency={credentialView.currency} loading={loading} onLoadOlder={() => void fetchEarlierDetail()} loadReplayArchive={loadReplayArchive} loadArchiveRange={loadArchiveRange} onSelect={(request) => { setDetail(undefined); setSelected(undefined); onOpenRequest(request); }} onClose={() => { setDetail(undefined); setSelected(undefined); }} />}
        </div>
      </div>
    </article>
  </div>;
}
