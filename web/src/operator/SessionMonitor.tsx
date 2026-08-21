import { useEffect, useRef, useState } from 'react';
import { api } from '../api';
import { useI18n } from '../i18n';
import { SessionDrawer, SessionList } from '../SessionViews';
import type {
  LogicalSessionCursor, LogicalSessionDetail, LogicalSessionListResponse, LogicalSessionSummary, RequestView,
} from '../types';

interface SessionFilters {
  q: string;
  keyId: string;
  model: string;
  state: '' | 'active' | 'has_errors';
}

export interface SessionFocus {
  sessionId: string;
  keyId: string;
  revision: number;
}

export type SessionStreamState = 'idle' | 'connecting' | 'live' | 'reconnecting';

const emptySessionFilters: SessionFilters = { q: '', keyId: '', model: '', state: '' };

function sessionsPath(tenant: string, filters: SessionFilters, before?: LogicalSessionCursor) {
  const params = new URLSearchParams({ tenant_external_id: tenant, limit: '50' });
  if (filters.q.trim()) params.set('q', filters.q.trim());
  if (filters.keyId.trim()) params.set('key_id', filters.keyId.trim());
  if (filters.model.trim()) params.set('model', filters.model.trim());
  if (filters.state) params.set('state', filters.state);
  if (before) {
    params.set('before_last_activity_at', String(before.before_last_activity_at));
    params.set('before_session_id', before.before_session_id);
  }
  return `/internal/v1/sessions?${params}`;
}

function detailPath(tenant: string, session: LogicalSessionSummary, cursor?: LogicalSessionDetail['next_cursor']) {
  const params = new URLSearchParams({ tenant_external_id: tenant, key_id: session.key_id, limit: '100' });
  if (cursor) {
    params.set('before_created_at', String(cursor.before_created_at));
    params.set('before_request_id', cursor.before_request_id);
  }
  return `/internal/v1/sessions/${encodeURIComponent(session.session_id)}?${params}`;
}

function messageOf(reason: unknown, fallback: string) {
  return reason instanceof Error ? reason.message : fallback;
}

export function SessionMonitor({ token, tenant, revision, eventKeyId, focus, streamState, onSelectRequest }: {
  token: string;
  tenant: string;
  revision: number;
  eventKeyId: string;
  focus?: SessionFocus;
  streamState: SessionStreamState;
  onSelectRequest: (request: RequestView) => Promise<void>;
}) {
  const { locale, t } = useI18n();
  const [sessions, setSessions] = useState<LogicalSessionSummary[]>([]);
  const [detail, setDetail] = useState<LogicalSessionDetail>();
  const [selected, setSelected] = useState<LogicalSessionSummary>();
  const [loading, setLoading] = useState(false);
  const [nextCursor, setNextCursor] = useState<LogicalSessionCursor | null>(null);
  const [generatedAt, setGeneratedAt] = useState(0);
  const [error, setError] = useState('');
  const [draft, setDraft] = useState<SessionFilters>(emptySessionFilters);
  const [filters, setFilters] = useState<SessionFilters>(emptySessionFilters);
  const [refreshing, setRefreshing] = useState(false);
  const listSequence = useRef(0);
  const detailSequence = useRef(0);
  const handledFocus = useRef(0);
  const firstPageSize = useRef(0);
  const loadedOlderList = useRef(false);
  const loadedOlderDetail = useRef(false);
  const refreshTimer = useRef<number | undefined>(undefined);
  const refreshDirty = useRef(false);
  const refreshInFlight = useRef(false);
  const dirtyKeyIds = useRef(new Set<string>());
  const scopeGeneration = useRef(0);
  const filtersRef = useRef(filters);
  const selectedRef = useRef<LogicalSessionSummary | undefined>(selected);
  filtersRef.current = filters;
  selectedRef.current = selected;

  async function loadSessions(older = false, selectedFilters = filters, background = false) {
    const sequence = ++listSequence.current;
    const credential = token.trim();
    if (!credential || !tenant) {
      setSessions([]); setNextCursor(null); setGeneratedAt(0);
      return;
    }
    if (!background) setLoading(true);
    else setRefreshing(true);
    setError('');
    try {
      const response = await api<LogicalSessionListResponse>(
        sessionsPath(tenant, selectedFilters, older ? nextCursor ?? undefined : undefined),
        credential,
      );
      if (sequence !== listSequence.current) return;
      const page = response.sessions;
      setSessions((current) => {
        if (background && loadedOlderList.current) {
          const keys = new Set(page.map((session) => `${session.key_id}:${session.session_id}`));
          const oldTail = current.slice(firstPageSize.current)
            .filter((session) => !keys.has(`${session.key_id}:${session.session_id}`));
          firstPageSize.current = page.length;
          return [...page, ...oldTail];
        }
        if (background) {
          firstPageSize.current = page.length;
          return page;
        }
        if (!older) {
          firstPageSize.current = page.length;
          loadedOlderList.current = false;
          return page;
        }
        loadedOlderList.current = true;
        const known = new Set(current.map((session) => `${session.key_id}:${session.session_id}`));
        return [...current, ...page.filter((session) => !known.has(`${session.key_id}:${session.session_id}`))];
      });
      if (!background || !loadedOlderList.current) setNextCursor(response.next_cursor);
      setGeneratedAt(response.generated_at);
      if (!older && focus && handledFocus.current !== focus.revision) {
        const focused = page.find((session) => session.session_id === focus.sessionId && session.key_id === focus.keyId);
        if (focused) {
          handledFocus.current = focus.revision;
          void selectSession(focused);
        }
      }
    } catch (reason) {
      if (sequence !== listSequence.current) return;
      setError(messageOf(reason, t('sessions.loadFailed')));
      if (!older) setSessions([]);
    } finally {
      if (sequence === listSequence.current) {
        setLoading(false);
        setRefreshing(false);
      }
    }
  }

  async function selectSession(session: LogicalSessionSummary) {
    const sequence = ++detailSequence.current;
    loadedOlderDetail.current = false;
    setSelected(session); setLoading(true); setError('');
    try {
      const next = await api<LogicalSessionDetail>(detailPath(tenant, session), token.trim());
      if (sequence === detailSequence.current) setDetail(next);
    } catch (reason) {
      if (sequence === detailSequence.current) setError(messageOf(reason, t('sessions.detailFailed')));
    } finally {
      if (sequence === detailSequence.current) setLoading(false);
    }
  }

  async function refreshSelected(session = selectedRef.current) {
    if (!session) return;
    const sequence = ++detailSequence.current;
    try {
      const page = await api<LogicalSessionDetail>(detailPath(tenant, session), token.trim());
      if (sequence !== detailSequence.current) return;
      setDetail((latest) => {
        if (!latest || latest.session_id !== page.session_id) return latest;
        if (!loadedOlderDetail.current) return page;
        const requests = new Map(latest.requests.map((request) => [request.request_id, request]));
        for (const request of page.requests) requests.set(request.request_id, request);
        const edges = new Map(latest.edges.map((edge) => [`${edge.from_request_id ?? ''}:${edge.to_request_id}:${edge.relation}`, edge]));
        for (const edge of page.edges) edges.set(`${edge.from_request_id ?? ''}:${edge.to_request_id}:${edge.relation}`, edge);
        return {
          ...page,
          requests: [...requests.values()].sort((left, right) => left.created_at - right.created_at || left.request_id.localeCompare(right.request_id)),
          edges: [...edges.values()],
          has_more: latest.has_more,
          next_cursor: latest.next_cursor,
          edges_truncated: page.edges_truncated || latest.edges_truncated,
        };
      });
    } catch (reason) {
      if (sequence === detailSequence.current) setError(messageOf(reason, t('sessions.detailFailed')));
    }
  }

  async function loadEarlier() {
    const current = detail;
    const session = selected;
    if (!current?.next_cursor || !session) return;
    const sequence = ++detailSequence.current;
    setLoading(true); setError('');
    try {
      const page = await api<LogicalSessionDetail>(detailPath(tenant, session, current.next_cursor), token.trim());
      if (sequence !== detailSequence.current) return;
      loadedOlderDetail.current = true;
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
      if (sequence === detailSequence.current) setError(messageOf(reason, t('sessions.detailFailed')));
    } finally {
      if (sequence === detailSequence.current) setLoading(false);
    }
  }

  function scheduleRefresh() {
    if (refreshTimer.current !== undefined || refreshInFlight.current) return;
    const generation = scopeGeneration.current;
    setRefreshing(true);
    refreshTimer.current = window.setTimeout(() => {
      if (generation !== scopeGeneration.current) return;
      refreshTimer.current = undefined;
      refreshInFlight.current = true;
      refreshDirty.current = false;
      const batchKeyIds = new Set(dirtyKeyIds.current);
      dirtyKeyIds.current.clear();
      const refresh = async () => {
        await loadSessions(false, filtersRef.current, true);
        const selectedSession = selectedRef.current;
        if (selectedSession && batchKeyIds.has(selectedSession.key_id)) await refreshSelected(selectedSession);
      };
      void refresh().finally(() => {
        if (generation !== scopeGeneration.current) return;
        refreshInFlight.current = false;
        if (refreshDirty.current || dirtyKeyIds.current.size > 0) scheduleRefresh();
      });
    }, 500);
  }

  useEffect(() => {
    if (!focus) return;
    const focusedFilters: SessionFilters = { q: focus.sessionId, keyId: focus.keyId, model: '', state: '' };
    setDraft(focusedFilters);
    setFilters(focusedFilters);
  }, [focus?.revision]);

  useEffect(() => {
    scopeGeneration.current += 1;
    if (refreshTimer.current !== undefined) window.clearTimeout(refreshTimer.current);
    refreshTimer.current = undefined;
    refreshDirty.current = false;
    refreshInFlight.current = false;
    dirtyKeyIds.current.clear();
    listSequence.current += 1;
    detailSequence.current += 1;
    firstPageSize.current = 0;
    loadedOlderList.current = false;
    loadedOlderDetail.current = false;
    setSessions([]); setDetail(undefined); setSelected(undefined); setNextCursor(null); setGeneratedAt(0);
    void loadSessions(false, filters);
    return () => {
      scopeGeneration.current += 1;
      if (refreshTimer.current !== undefined) window.clearTimeout(refreshTimer.current);
      refreshTimer.current = undefined;
      refreshDirty.current = false;
      dirtyKeyIds.current.clear();
      listSequence.current += 1;
      detailSequence.current += 1;
    };
  }, [token, tenant, filters]);

  useEffect(() => {
    if (!token.trim() || !tenant || revision === 0) return;
    if (eventKeyId) dirtyKeyIds.current.add(eventKeyId);
    refreshDirty.current = true;
    scheduleRefresh();
  }, [revision, eventKeyId]);

  const status = refreshing ? 'refreshing' : streamState === 'idle' ? 'connecting' : streamState;
  return <>
    {!tenant && <div className="notice warning" role="status">{t('sessions.selectTenant')}</div>}
    {error && <div className="notice error" role="alert">{error}</div>}
    <div className={`session-live-state ${status}`} role="status">{t(`sessions.live.${status}`)}</div>
    <form className="session-controls" onSubmit={(event) => { event.preventDefault(); setFilters({ ...draft }); }}>
      <label>{t('sessions.search')}<input value={draft.q} onChange={(event) => setDraft({ ...draft, q: event.target.value })} placeholder={t('sessions.searchPlaceholder')} /></label>
      <label>{t('traffic.keyId')}<input value={draft.keyId} onChange={(event) => setDraft({ ...draft, keyId: event.target.value })} placeholder="019f…" /></label>
      <label>{t('request.model')}<input value={draft.model} onChange={(event) => setDraft({ ...draft, model: event.target.value })} /></label>
      <label>{t('sessions.state')}<select value={draft.state} onChange={(event) => setDraft({ ...draft, state: event.target.value as SessionFilters['state'] })}><option value="">{t('common.all')}</option><option value="active">{t('sessions.filter.active')}</option><option value="has_errors">{t('sessions.filter.hasErrors')}</option></select></label>
      <div className="filter-actions"><button type="submit" disabled={loading || !tenant}>{t('traffic.applyFilters')}</button><button type="button" className="secondary" disabled={loading || !Object.values(filters).some(Boolean)} onClick={() => { setDraft(emptySessionFilters); setFilters(emptySessionFilters); }}>{t('traffic.clearFilters')}</button></div>
    </form>
    <p className="muted session-result-count">{t('sessions.serverFiltered', { count: sessions.length })}{generatedAt > 0 && <> · {t('sessions.generatedAt', { time: new Date(generatedAt).toLocaleString(locale) })}</>}</p>
    <SessionList values={sessions} loading={loading} showCredential onSelect={(session) => void selectSession(session)} />
    {nextCursor && <div className="load-more"><button type="button" className="secondary" disabled={loading} onClick={() => void loadSessions(true, filters)}>{loading ? t('common.loading') : t('sessions.loadOlder')}</button></div>}
    {detail && <SessionDrawer detail={detail} summary={selected} showDiagnosticIds loading={loading} onLoadOlder={() => void loadEarlier()} onSelect={(request) => { setDetail(undefined); setSelected(undefined); void onSelectRequest(request); }} onClose={() => { setDetail(undefined); setSelected(undefined); }} />}
  </>;
}
