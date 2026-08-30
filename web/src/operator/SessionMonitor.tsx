import { useEffect, useRef, useState, type RefObject } from 'react';
import { api } from '../api.js';
import { useI18n } from '../i18n.js';
import { SessionDrawer, SessionList } from '../SessionViews.js';
import { drainSessionEventKeys, mergeSessionPage } from './sessionRefresh.js';
import type {
  LogicalSessionCursor, LogicalSessionDetail, LogicalSessionListResponse, LogicalSessionSummary, RequestView,
} from '../types.js';

interface SessionFilters {
  q: string;
  keyId: string;
  model: string;
  state: '' | 'active' | 'has_errors';
}

export interface SessionFocus {
  sessionId: string;
  keyId?: string;
  revision: number;
}

export type SessionStreamState = 'idle' | 'connecting' | 'live' | 'reconnecting';

export interface LatestRequest {
  signal: AbortSignal;
  isCurrent: () => boolean;
}

/** Owns one request lane: starting B aborts A, and invalidating a scope rejects both. */
export class LatestRequestGate {
  private sequence = 0;
  private controller?: AbortController;

  begin(): LatestRequest {
    this.controller?.abort();
    const controller = new AbortController();
    const sequence = ++this.sequence;
    this.controller = controller;
    return {
      signal: controller.signal,
      isCurrent: () => sequence === this.sequence && !controller.signal.aborted,
    };
  }

  invalidate() {
    this.sequence += 1;
    this.controller?.abort();
    this.controller = undefined;
  }
}

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

export function SessionMonitor({ token, tenant, revision, eventKeyIds, focus, streamState, onSelectRequest }: {
  token: string;
  tenant: string;
  revision: number;
  eventKeyIds: RefObject<Set<string>>;
  focus?: SessionFocus;
  streamState: SessionStreamState;
  onSelectRequest: (request: RequestView) => Promise<void>;
}) {
  const { locale, t } = useI18n();
  const scopeKey = `${tenant}\0${token}`;
  const [sessions, setSessions] = useState<LogicalSessionSummary[]>([]);
  const [listScope, setListScope] = useState('');
  const [detail, setDetail] = useState<LogicalSessionDetail>();
  const [detailScope, setDetailScope] = useState('');
  const [selected, setSelected] = useState<LogicalSessionSummary>();
  const [loading, setLoading] = useState(false);
  const [nextCursor, setNextCursor] = useState<LogicalSessionCursor | null>(null);
  const [generatedAt, setGeneratedAt] = useState(0);
  const [error, setError] = useState('');
  const [errorScope, setErrorScope] = useState('');
  const [draft, setDraft] = useState<SessionFilters>(emptySessionFilters);
  const [filters, setFilters] = useState<SessionFilters>(emptySessionFilters);
  const [refreshing, setRefreshing] = useState(false);
  const listSequence = useRef(0);
  const detailRequests = useRef(new LatestRequestGate());
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
    const requestScope = scopeKey;
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
      const resetActiveTail = background && loadedOlderList.current && selectedFilters.state === 'active';
      setSessions((current) => {
        const merged = mergeSessionPage({
          current,
          page,
          firstPageSize: firstPageSize.current,
          loadedOlder: loadedOlderList.current,
          older,
          background,
          state: selectedFilters.state,
        });
        firstPageSize.current = merged.firstPageSize;
        loadedOlderList.current = merged.loadedOlder;
        return merged.sessions;
      });
      setListScope(requestScope);
      if (!background || !loadedOlderList.current || resetActiveTail) setNextCursor(response.next_cursor);
      setGeneratedAt(response.generated_at);
      if (!older && focus && handledFocus.current !== focus.revision) {
        const focused = page.find((session) => session.session_id === focus.sessionId && (!focus.keyId || session.key_id === focus.keyId));
        if (focused) {
          handledFocus.current = focus.revision;
          void selectSession(focused);
        }
      }
    } catch (reason) {
      if (sequence !== listSequence.current) return;
      setError(messageOf(reason, t('sessions.loadFailed')));
      setErrorScope(requestScope);
      if (!older) setSessions([]);
    } finally {
      if (sequence === listSequence.current) {
        setLoading(false);
        setRefreshing(false);
      }
    }
  }

  async function selectSession(session: LogicalSessionSummary) {
    const request = detailRequests.current.begin();
    const requestScope = scopeKey;
    loadedOlderDetail.current = false;
    setSelected(session); setDetail(undefined); setDetailScope(''); setLoading(true); setError(''); setErrorScope('');
    try {
      const next = await api<LogicalSessionDetail>(detailPath(tenant, session), token.trim(), { signal: request.signal });
      if (request.isCurrent()) {
        setDetail(next);
        setDetailScope(requestScope);
      }
    } catch (reason) {
      if (request.isCurrent()) {
        setError(messageOf(reason, t('sessions.detailFailed')));
        setErrorScope(requestScope);
      }
    } finally {
      if (request.isCurrent()) setLoading(false);
    }
  }

  async function refreshSelected(session = selectedRef.current) {
    if (!session) return;
    const request = detailRequests.current.begin();
    const requestScope = scopeKey;
    try {
      const page = await api<LogicalSessionDetail>(detailPath(tenant, session), token.trim(), { signal: request.signal });
      if (!request.isCurrent()) return;
      setDetail((latest) => {
        if (!latest) return page;
        if (latest.session_id !== page.session_id) return latest;
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
      setDetailScope(requestScope);
    } catch (reason) {
      if (request.isCurrent()) {
        setError(messageOf(reason, t('sessions.detailFailed')));
        setErrorScope(requestScope);
      }
    } finally {
      if (request.isCurrent()) setLoading(false);
    }
  }

  async function loadEarlier() {
    const current = detail;
    const session = selected;
    if (!current?.next_cursor || !session) return;
    const request = detailRequests.current.begin();
    const requestScope = scopeKey;
    setLoading(true); setError('');
    try {
      const page = await api<LogicalSessionDetail>(detailPath(tenant, session, current.next_cursor), token.trim(), { signal: request.signal });
      if (!request.isCurrent()) return;
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
      setDetailScope(requestScope);
    } catch (reason) {
      if (request.isCurrent()) {
        setError(messageOf(reason, t('sessions.detailFailed')));
        setErrorScope(requestScope);
      }
    } finally {
      if (request.isCurrent()) setLoading(false);
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
    const focusedFilters: SessionFilters = { q: focus.sessionId, keyId: focus.keyId ?? '', model: '', state: '' };
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
    detailRequests.current.invalidate();
    firstPageSize.current = 0;
    loadedOlderList.current = false;
    loadedOlderDetail.current = false;
    handledFocus.current = 0;
    setSessions([]); setListScope(''); setDetail(undefined); setDetailScope(''); setSelected(undefined); setNextCursor(null); setGeneratedAt(0);
    setLoading(false); setRefreshing(false); setError(''); setErrorScope('');
    if (!token.trim() || !tenant) {
      setDraft(emptySessionFilters);
      setFilters(emptySessionFilters);
    }
    void loadSessions(false, filters);
    return () => {
      scopeGeneration.current += 1;
      if (refreshTimer.current !== undefined) window.clearTimeout(refreshTimer.current);
      refreshTimer.current = undefined;
      refreshDirty.current = false;
      refreshInFlight.current = false;
      dirtyKeyIds.current.clear();
      listSequence.current += 1;
      detailRequests.current.invalidate();
    };
  }, [token, tenant, filters]);

  useEffect(() => {
    if (!token.trim() || !tenant || revision === 0) return;
    for (const keyId of drainSessionEventKeys(eventKeyIds.current)) dirtyKeyIds.current.add(keyId);
    refreshDirty.current = true;
    scheduleRefresh();
  }, [revision, eventKeyIds]);

  const hasScope = Boolean(token.trim() && tenant);
  const status = !hasScope ? 'idle' : refreshing ? 'refreshing' : streamState;
  const visibleSessions = listScope === scopeKey ? sessions : [];
  const visibleDetail = detailScope === scopeKey ? detail : undefined;
  const visibleError = errorScope === scopeKey ? error : '';
  return <>
    {!tenant && <div className="notice warning" role="status">{t('sessions.selectTenant')}</div>}
    {visibleError && <div className="notice error" role="alert">{visibleError}</div>}
    <div className={`session-live-state ${status}`} role="status">{t(`sessions.live.${status}`)}</div>
    <form className="session-controls" onSubmit={(event) => { event.preventDefault(); setFilters({ ...draft }); }}>
      <label>{t('sessions.search')}<input value={draft.q} onChange={(event) => setDraft({ ...draft, q: event.target.value })} placeholder={t('sessions.searchPlaceholder')} /></label>
      <label>{t('traffic.keyId')}<input value={draft.keyId} onChange={(event) => setDraft({ ...draft, keyId: event.target.value })} placeholder="019f…" /></label>
      <label>{t('request.model')}<input value={draft.model} onChange={(event) => setDraft({ ...draft, model: event.target.value })} /></label>
      <label>{t('sessions.state')}<select value={draft.state} onChange={(event) => setDraft({ ...draft, state: event.target.value as SessionFilters['state'] })}><option value="">{t('common.all')}</option><option value="active">{t('sessions.filter.active')}</option><option value="has_errors">{t('sessions.filter.hasErrors')}</option></select></label>
      <div className="filter-actions"><button type="submit" disabled={loading || !tenant}>{t('traffic.applyFilters')}</button><button type="button" className="secondary" disabled={loading || !Object.values(filters).some(Boolean)} onClick={() => { setDraft(emptySessionFilters); setFilters(emptySessionFilters); }}>{t('traffic.clearFilters')}</button></div>
    </form>
    <p className="muted session-result-count">{t('sessions.serverFiltered', { count: visibleSessions.length })}{listScope === scopeKey && generatedAt > 0 && <> · {t('sessions.generatedAt', { time: new Date(generatedAt).toLocaleString(locale) })}</>}</p>
    <SessionList values={visibleSessions} loading={loading} showCredential onSelect={(session) => void selectSession(session)} />
    {listScope === scopeKey && nextCursor && <div className="load-more"><button type="button" className="secondary" disabled={loading} onClick={() => void loadSessions(true, filters)}>{loading ? t('common.loading') : t('sessions.loadOlder')}</button></div>}
    {visibleDetail && <SessionDrawer detail={visibleDetail} summary={selected} showDiagnosticIds loading={loading} onLoadOlder={() => void loadEarlier()} onSelect={(request) => { setDetail(undefined); setDetailScope(''); setSelected(undefined); void onSelectRequest(request); }} onClose={() => { setDetail(undefined); setDetailScope(''); setSelected(undefined); }} />}
  </>;
}
