import { useCallback, useEffect, useRef, useState, type RefObject } from 'react';
import { api, apiRead } from '../api.js';
import { readArchiveRange, type ArchiveRangeLoader } from '../archiveRange.js';
import { useI18n } from '../i18n.js';
import { Button, Checkbox } from '../design-system';
import { SessionDetailSurface, SessionList } from '../SessionViews.js';
import { SessionCredentialFilter } from './SessionCredentialFilter.js';
import {
  drainSessionEventIdentities, mergeSessionPage, sessionEventsRequireDetailRefresh, sessionEventTargetsSelection,
  sessionEventRefreshDelayMs, sessionIdentityKey,
} from './sessionRefresh.js';
import { LatestRequestGate } from './latestRequestGate.js';
import type {
  LogicalSessionCursor, LogicalSessionDetail, LogicalSessionListResponse, LogicalSessionSummary, RequestDetail, RequestView,
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

export { LatestRequestGate, type LatestRequest } from './latestRequestGate.js';

const emptySessionFilters: SessionFilters = { q: '', keyId: '', model: '', state: '' };

function sessionsPath(tenant: string, filters: SessionFilters, before?: LogicalSessionCursor) {
  const params = new URLSearchParams({ limit: '50' });
  if (tenant) params.set('tenant_external_id', tenant);
  if (filters.q.trim()) params.set('q', filters.q.trim());
  if (filters.keyId.trim()) params.set('key_id', filters.keyId.trim());
  if (filters.model.trim()) params.set('model', filters.model.trim());
  if (filters.state) params.set('state', filters.state);
  if (before) {
    params.set('before_last_activity_at', String(before.before_last_activity_at));
    params.set('before_session_id', before.before_session_id);
    params.set('before_key_id', before.before_key_id);
  }
  return `/internal/v1/sessions?${params}`;
}

function detailPath(tenant: string, session: LogicalSessionSummary, cursor?: LogicalSessionDetail['next_cursor']) {
  const params = new URLSearchParams({ key_id: session.key_id, limit: '100' });
  if (tenant) params.set('tenant_external_id', tenant);
  if (cursor) {
    params.set('before_created_at', String(cursor.before_created_at));
    params.set('before_request_id', cursor.before_request_id);
  }
  return `/internal/v1/sessions/${encodeURIComponent(session.session_id)}?${params}`;
}

function requestArchivePath(tenant: string, requestId: string, side?: 'request' | 'response') {
  const params = new URLSearchParams();
  if (tenant) params.set('tenant_external_id', tenant);
  const query = params.toString();
  return `/internal/v1/requests/${encodeURIComponent(requestId)}${side ? `/archive/${side}` : ''}${query ? `?${query}` : ''}`;
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
  const [detailLoading, setDetailLoading] = useState(false);
  const [nextCursor, setNextCursor] = useState<LogicalSessionCursor | null>(null);
  const [generatedAt, setGeneratedAt] = useState(0);
  const [error, setError] = useState('');
  const [errorScope, setErrorScope] = useState('');
  const [draft, setDraft] = useState<SessionFilters>(emptySessionFilters);
  const [filters, setFilters] = useState<SessionFilters>(emptySessionFilters);
  const [refreshing, setRefreshing] = useState(false);
  const [autoRefresh, setAutoRefresh] = useState(false);
  const autoRefreshRef = useRef(autoRefresh);
  autoRefreshRef.current = autoRefresh;
  const listSequence = useRef(0);
  const listRequests = useRef(new LatestRequestGate());
  const listInFlight = useRef(false);
  const detailRequests = useRef(new LatestRequestGate());
  const detailInFlight = useRef(false);
  const handledFocus = useRef(0);
  const firstPageSize = useRef(0);
  const loadedOlderList = useRef(false);
  const loadedOlderDetail = useRef(false);
  const refreshTimer = useRef<number | undefined>(undefined);
  const refreshDirty = useRef(false);
  const refreshInFlight = useRef(false);
  const refreshCancelled = useRef(false);
  const dirtyEventIdentities = useRef(new Set<string>());
  const dirtyDetailEvents = useRef(new Set<string>());
  const detailRefreshDirty = useRef(false);
  const scopeGeneration = useRef(0);
  const filtersRef = useRef(filters);
  const selectedRef = useRef<LogicalSessionSummary | undefined>(selected);
  const detailRef = useRef<LogicalSessionDetail | undefined>(detail);
  filtersRef.current = filters;
  selectedRef.current = selected;
  detailRef.current = detail;
  const loadReplayArchive = useCallback((request: RequestView, signal: AbortSignal) => api<RequestDetail>(
    requestArchivePath(tenant, request.request_id), token.trim(), { signal },
  ), [tenant, token]);
  const loadArchiveRange = useCallback<ArchiveRangeLoader>((requestId, side, offset, length, etag, signal) => readArchiveRange(
    requestArchivePath(tenant, requestId, side), token.trim(), offset, length, etag, AbortSignal.any([signal, AbortSignal.timeout(15_000)]),
  ), [tenant, token]);

  async function loadSessions(older = false, selectedFilters = filters, background = false) {
    const sequence = ++listSequence.current;
    const request = listRequests.current.begin();
    const requestScope = scopeKey;
    const credential = token.trim();
    if (!credential) {
      setSessions([]); setNextCursor(null); setGeneratedAt(0);
      return false;
    }
    if (!background) setLoading(true);
    else setRefreshing(true);
    listInFlight.current = true;
    setError('');
    try {
      // A list read is idempotent. Retry early transient failures inside the
      // existing 15-second user-visible budget; scope/filter cancellation still
      // immediately aborts every attempt and its backoff.
      const response = await apiRead<LogicalSessionListResponse>(
        sessionsPath(tenant, selectedFilters, older ? nextCursor ?? undefined : undefined),
        credential,
        { attempts: 3, attemptTimeoutMilliseconds: 15_000, totalTimeoutMilliseconds: 15_000, signal: request.signal },
      );
      if (!request.isCurrent() || sequence !== listSequence.current) return false;
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
      if (!older && !background) {
        const pendingFocus = focus && handledFocus.current !== focus.revision ? focus : undefined;
        const focused = pendingFocus
          ? page.find((session) => session.session_id === pendingFocus.sessionId && (!pendingFocus.keyId || session.key_id === pendingFocus.keyId))
          : undefined;
        const currentlyVisible = selectedRef.current && page.some((session) => session.session_id === selectedRef.current?.session_id && session.key_id === selectedRef.current?.key_id);
        const target = focused ?? (!pendingFocus && !currentlyVisible ? page[0] : undefined);
        if (focused && pendingFocus) handledFocus.current = pendingFocus.revision;
        if (target) void selectSession(target);
      }
      return true;
    } catch (reason) {
      if (!request.isCurrent() || sequence !== listSequence.current) return false;
      setError(messageOf(reason, t('sessions.loadFailed')));
      setErrorScope(requestScope);
      // A failed live refresh is not an empty result set. Keep the current
      // scoped page (including older pages) and report the refresh failure.
      // Keep an already rendered page while a same-scope manual retry fails.
      // Scope/filter transitions clear their own state before starting a read.
      return false;
    } finally {
      if (request.isCurrent() && sequence === listSequence.current) {
        listInFlight.current = false;
        setLoading(false);
        setRefreshing(false);
        if (!background && refreshDirty.current) scheduleRefresh();
      }
    }
  }

  async function selectSession(session: LogicalSessionSummary) {
    const request = detailRequests.current.begin();
    detailInFlight.current = true;
    selectedRef.current = session;
    const requestScope = scopeKey;
    loadedOlderDetail.current = false;
    setSelected(session); setDetail(undefined); setDetailScope(''); setDetailLoading(true); setError(''); setErrorScope('');
    try {
      const next = await apiRead<LogicalSessionDetail>(detailPath(tenant, session), token.trim(), {
        attempts: 3, attemptTimeoutMilliseconds: 15_000, totalTimeoutMilliseconds: 15_000, signal: request.signal,
      });
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
      if (request.isCurrent()) { detailInFlight.current = false; setDetailLoading(false); if (detailRefreshDirty.current) scheduleRefresh(); }
    }
  }

  async function refreshSelected(session = selectedRef.current) {
    if (!session) return false;
    if (detailInFlight.current) { detailRefreshDirty.current = true; return false; }
    const request = detailRequests.current.begin();
    detailInFlight.current = true;
    const requestScope = scopeKey;
    try {
      const page = await apiRead<LogicalSessionDetail>(detailPath(tenant, session), token.trim(), {
        attempts: 3, attemptTimeoutMilliseconds: 15_000, totalTimeoutMilliseconds: 15_000, signal: request.signal,
      });
      if (!request.isCurrent()) return false;
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
      return true;
    } catch (reason) {
      if (request.isCurrent()) {
        setError(messageOf(reason, t('sessions.detailFailed')));
        setErrorScope(requestScope);
      }
      return false;
    } finally {
      if (request.isCurrent()) { detailInFlight.current = false; setDetailLoading(false); if (detailRefreshDirty.current) scheduleRefresh(); }
    }
  }

  async function loadEarlier() {
    const current = detail;
    const session = selected;
    if (!current?.next_cursor || !session || detailInFlight.current) return;
    const request = detailRequests.current.begin();
    detailInFlight.current = true;
    const requestScope = scopeKey;
    setDetailLoading(true); setError('');
    try {
      const page = await apiRead<LogicalSessionDetail>(detailPath(tenant, session, current.next_cursor), token.trim(), {
        attempts: 3, attemptTimeoutMilliseconds: 15_000, totalTimeoutMilliseconds: 15_000, signal: request.signal,
      });
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
      if (request.isCurrent()) { detailInFlight.current = false; setDetailLoading(false); if (detailRefreshDirty.current) scheduleRefresh(); }
    }
  }

  function scheduleRefresh() {
    // Do not launch a second full list read while its initial/manual page is
    // still loading. The dirty set is drained once that read has settled.
    if (!autoRefreshRef.current || refreshTimer.current !== undefined || refreshInFlight.current || listInFlight.current) return;
    const generation = scopeGeneration.current;
    setRefreshing(true);
    refreshTimer.current = window.setTimeout(() => {
      refreshTimer.current = undefined;
      if (generation !== scopeGeneration.current || !autoRefreshRef.current) return;
      refreshInFlight.current = true;
      refreshDirty.current = false;
      const batchEventIdentities = new Set(dirtyEventIdentities.current);
      dirtyEventIdentities.current.clear();
      const batchDetailEvents = new Set(dirtyDetailEvents.current);
      dirtyDetailEvents.current.clear();
      const selectedAtBatchStart = selectedRef.current;
      const selectedIdentity = selectedAtBatchStart ? sessionIdentityKey(selectedAtBatchStart) : undefined;
      const restoreBatch = () => {
        refreshDirty.current = true;
        for (const identity of batchEventIdentities) dirtyEventIdentities.current.add(identity);
        for (const identity of batchDetailEvents) dirtyDetailEvents.current.add(identity);
      };
      const refresh = async () => {
        const listLoaded = await loadSessions(false, filtersRef.current, true);
        if (generation !== scopeGeneration.current || !autoRefreshRef.current) return;
        if (!listLoaded) { restoreBatch(); return; }
        const latestSelection = selectedRef.current;
        if (selectedAtBatchStart && latestSelection && sessionIdentityKey(latestSelection) === selectedIdentity
          && sessionEventsRequireDetailRefresh(batchDetailEvents, latestSelection, detailRef.current)) {
          if (detailInFlight.current) {
            for (const event of batchDetailEvents) dirtyDetailEvents.current.add(event);
            detailRefreshDirty.current = true;
            refreshDirty.current = true;
            return;
          }
          detailRefreshDirty.current = false;
          if (!await refreshSelected(selectedAtBatchStart)) restoreBatch();
        }
      };
      void refresh().finally(() => {
        if (generation !== scopeGeneration.current) return;
        refreshInFlight.current = false;
        if (!refreshCancelled.current && !detailInFlight.current
          && (refreshDirty.current || dirtyEventIdentities.current.size > 0)) scheduleRefresh();
      });
    }, sessionEventRefreshDelayMs);
  }

  useEffect(() => {
    if (!focus) return;
    const focusedFilters: SessionFilters = { q: focus.sessionId, keyId: focus.keyId ?? '', model: '', state: '' };
    setDraft(focusedFilters);
    setFilters(focusedFilters);
  }, [focus?.revision]);

  useEffect(() => {
    scopeGeneration.current += 1;
    // A filter/scope transition starts an authoritative first-page read. Drop
    // events queued for the previous projection before issuing that snapshot;
    // otherwise the next unrelated revision drains both batches and can make
    // an old same-session event look as if it belonged to the new event.
    // Events arriving after this synchronous boundary receive a new revision
    // and are processed against the in-flight snapshot normally.
    eventKeyIds.current.clear();
    if (refreshTimer.current !== undefined) window.clearTimeout(refreshTimer.current);
    refreshTimer.current = undefined;
    refreshDirty.current = false;
    refreshInFlight.current = false;
    refreshCancelled.current = false;
    dirtyEventIdentities.current.clear();
    dirtyDetailEvents.current.clear();
    detailRefreshDirty.current = false;
    listSequence.current += 1;
    listRequests.current.invalidate();
    listInFlight.current = false;
    detailRequests.current.invalidate();
    detailInFlight.current = false;
    firstPageSize.current = 0;
    loadedOlderList.current = false;
    loadedOlderDetail.current = false;
    handledFocus.current = 0;
    setSessions([]); setListScope(''); setDetail(undefined); setDetailScope(''); setSelected(undefined); setNextCursor(null); setGeneratedAt(0);
    setLoading(false); setDetailLoading(false); setRefreshing(false); setError(''); setErrorScope('');
    if (!token.trim()) {
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
      refreshCancelled.current = false;
      dirtyEventIdentities.current.clear();
      dirtyDetailEvents.current.clear();
      detailRefreshDirty.current = false;
      listSequence.current += 1;
      listRequests.current.invalidate();
      listInFlight.current = false;
      detailRequests.current.invalidate();
    };
  }, [token, tenant, filters]);

  useEffect(() => {
    if (!token.trim() || revision === 0) return;
    refreshCancelled.current = false;
    const eventIdentities = drainSessionEventIdentities(eventKeyIds.current);
    if (!autoRefreshRef.current) return;
    for (const identity of eventIdentities) dirtyEventIdentities.current.add(identity);
    const selectedAtEvent = selectedRef.current;
    if (selectedAtEvent && sessionEventTargetsSelection(eventIdentities, selectedAtEvent)) {
      for (const identity of eventIdentities) dirtyDetailEvents.current.add(identity);
    }
    refreshDirty.current = true;
    scheduleRefresh();
  }, [revision, eventKeyIds]);

  function toggleAutoRefresh(enabled: boolean) {
    autoRefreshRef.current = enabled;
    setAutoRefresh(enabled);
    if (enabled) { refreshCancelled.current = false; refreshDirty.current = true; scheduleRefresh(); }
    else {
      if (refreshTimer.current !== undefined) window.clearTimeout(refreshTimer.current);
      refreshTimer.current = undefined;
      refreshDirty.current = false;
      dirtyEventIdentities.current.clear();
      dirtyDetailEvents.current.clear();
      if (!refreshInFlight.current) setRefreshing(false);
    }
  }

  function cancelListLoad() {
    if (!listInFlight.current) return;
    listSequence.current += 1;
    listRequests.current.invalidate();
    listInFlight.current = false;
    // Cancellation only stops the currently owned request. Events that arrived
    // in this scope still describe server state we have not observed, so keep
    // their batch for the next manual refresh or stream invalidation. Clear
    // only the scheduling latch so refresh().finally cannot immediately
    // replace a user-cancelled request with another one.
    refreshDirty.current = false;
    refreshCancelled.current = true;
    setLoading(false);
    setRefreshing(false);
  }

  const hasScope = Boolean(token.trim());
  const status = !hasScope ? 'idle' : refreshing ? 'refreshing' : streamState;
  const visibleSessions = listScope === scopeKey ? sessions : [];
  const visibleDetail = detailScope === scopeKey ? detail : undefined;
  const visibleError = errorScope === scopeKey ? error : '';
  return <>
    {visibleError && <div className="notice error" role="alert">{visibleError} <button type="button" className="secondary" disabled={loading} onClick={() => void loadSessions(false, filters, visibleSessions.length > 0)}>{t('sessions.retryLoad')}</button></div>}
    <div className="session-refresh-controls">
      <Checkbox checked={autoRefresh} onChange={(_, data) => toggleAutoRefresh(data.checked === true)} label={t('sessions.autoRefresh')} />
      <span className={`session-live-state ${status}`} role="status">{autoRefresh ? t(`sessions.live.${status}`) : t('sessions.paused')}</span>
      <Button appearance="secondary" disabled={loading || refreshing || detailLoading} onClick={() => { void loadSessions(false, filters, visibleSessions.length > 0); if (selected) void refreshSelected(selected); }}>{t('sessions.refreshNow')}</Button>
      {(loading || refreshing) && <Button appearance="secondary" onClick={cancelListLoad}>{t('common.cancel')}</Button>}
    </div>
    <form className="session-controls" onSubmit={(event) => { event.preventDefault(); setFilters({ ...draft }); }}>
      <label>{t('sessions.search')}<input value={draft.q} onChange={(event) => setDraft({ ...draft, q: event.target.value })} placeholder={t('sessions.searchPlaceholder')} /></label>
      <SessionCredentialFilter value={draft.keyId} sessions={visibleSessions} token={token} tenant={tenant} onChange={(keyId) => setDraft({ ...draft, keyId })} />
      <label>{t('request.model')}<input value={draft.model} onChange={(event) => setDraft({ ...draft, model: event.target.value })} /></label>
      <label>{t('sessions.state')}<select value={draft.state} onChange={(event) => setDraft({ ...draft, state: event.target.value as SessionFilters['state'] })}><option value="">{t('common.all')}</option><option value="active">{t('sessions.filter.active')}</option><option value="has_errors">{t('sessions.filter.hasErrors')}</option></select></label>
      <div className="filter-actions"><button type="submit" disabled={loading}>{t('traffic.applyFilters')}</button><button type="button" className="secondary" disabled={loading || !Object.values(filters).some(Boolean)} onClick={() => { setDraft(emptySessionFilters); setFilters(emptySessionFilters); }}>{t('traffic.clearFilters')}</button></div>
    </form>
    <p className="muted session-result-count">{loading && !visibleSessions.length ? t('common.loading') : t('sessions.serverFiltered', { count: visibleSessions.length })}{listScope === scopeKey && generatedAt > 0 && <> · {t('sessions.generatedAt', { time: new Date(generatedAt).toLocaleString(locale) })}</>}</p>
    <div className="session-workspace">
      <section className="session-browser" aria-label={t('sessions.recent')}>
        <SessionList values={visibleSessions} loading={loading} showCredential selected={selected} layout="sidebar" onSelect={(session) => void selectSession(session)} />
        {listScope === scopeKey && nextCursor && <div className="load-more"><Button appearance="secondary" disabled={loading} onClick={() => void loadSessions(true, filters)}>{loading ? t('common.loading') : t('sessions.loadOlder')}</Button></div>}
      </section>
      <div className="session-detail-region">
        {!visibleDetail && detailLoading && <div className="empty" role="status">{t('common.loading')}</div>}
        {!visibleDetail && !detailLoading && <div className="empty">{selected && visibleError ? <button type="button" className="secondary" onClick={() => void selectSession(selected)}>{t('sessions.retryLoad')}</button> : t('sessions.selectHint')}</div>}
        {visibleDetail && <SessionDetailSurface detail={visibleDetail} summary={selected} showDiagnosticIds loading={detailLoading} onLoadOlder={() => void loadEarlier()} loadReplayArchive={loadReplayArchive} loadArchiveRange={loadArchiveRange} onSelect={(request) => { void onSelectRequest(request); }} onClose={() => { detailRequests.current.invalidate(); detailInFlight.current = false; setDetailLoading(false); setDetail(undefined); setDetailScope(''); setSelected(undefined); selectedRef.current = undefined; }} />}
      </div>
    </div>
  </>;
}
