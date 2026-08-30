import { useEffect, useRef, useState } from 'react';
import { api } from '../api';
import { formatNumber } from '../format';
import { useI18n } from '../i18n';
import { SessionDrawer, SessionList } from '../SessionViews';
import type { KeyView, LogicalSessionCursor, LogicalSessionDetail, LogicalSessionListResponse, LogicalSessionSummary, RequestView } from '../types';
import { selfErrorMessage } from './errors';
import { sessionDetailPath, sessionsPath } from './requestPaths';

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
  const listSequence = useRef(0);
  const detailSequence = useRef(0);
  const listController = useRef<AbortController | undefined>(undefined);
  const detailController = useRef<AbortController | undefined>(undefined);

  async function fetchSessions(before?: LogicalSessionCursor) {
    const sequence = ++listSequence.current;
    listController.current?.abort();
    const controller = new AbortController();
    listController.current = controller;
    setLoading(true);
    onError('');
    try {
      const response = await api<LogicalSessionListResponse>(sessionsPath(before, before ? undefined : focusSessionId), credential, { signal: controller.signal });
      if (sequence !== listSequence.current || controller.signal.aborted) return;
      setSessions((current) => {
        if (!before) return response.sessions;
        const known = new Set(current.map((session) => session.session_id));
        return [...current, ...response.sessions.filter((session) => !known.has(session.session_id))];
      });
      setNextCursor(response.next_cursor);
      setGeneratedAt(response.generated_at);
      if (!before && focusSessionId) {
        const focused = response.sessions.find((session) => session.session_id === focusSessionId);
        if (focused) void selectSession(focused);
        else onError(t('self.resourceMissing'));
      }
    } catch (reason) {
      if (sequence === listSequence.current && !controller.signal.aborted) onError(selfErrorMessage(reason, t, t('common.requestFailed')));
    } finally {
      if (sequence === listSequence.current) setLoading(false);
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
    setLoading(true);
    void fetchSessions();
    return () => {
      listSequence.current += 1;
      detailSequence.current += 1;
      listController.current?.abort();
      detailController.current?.abort();
    };
  }, [credential, focusSessionId]);

  return <div className="self-page self-sessions-page" data-self-page="sessions">
    <article className="panel self-sessions">
      <div className="panel-title"><div><h2>{t('sessions.selfTitle')}</h2></div><span>{t('sessions.loaded', { count: formatNumber(sessions.length, locale) })}{generatedAt > 0 && ` · ${t('sessions.generatedAt', { time: new Date(generatedAt).toLocaleString(locale) })}`}</span></div>
      <SessionList values={sessions} loading={loading} showCredential={false} onSelect={(session) => void selectSession(session)} />
      {nextCursor && <div className="load-more"><button type="button" className="secondary" disabled={loading} onClick={() => void fetchSessions(nextCursor)}>{loading ? t('common.loading') : t('sessions.loadOlder')}</button></div>}
    </article>
    {detail && <SessionDrawer detail={detail} summary={selected} currency={credentialView.currency} loading={loading} onLoadOlder={() => void fetchEarlierDetail()} onSelect={(request) => { setDetail(undefined); setSelected(undefined); onOpenRequest(request); }} onClose={() => { setDetail(undefined); setSelected(undefined); }} />}
  </div>;
}
