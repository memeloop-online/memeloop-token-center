import { useEffect, useRef, useState } from 'react';
import { api } from '../../api';
import { DrawerFrame, Metric, NumberMetric, RequestDiagnostics, RequestTable } from '../../components';
import { formatMilliseconds, formatPercent } from '../../format';
import { useI18n } from '../../i18n';
import type { RequestDetail, RequestEvent, RequestListResponse, RequestView, TypedFilterAst, UpstreamAccount } from '../../types';
import type { SessionStreamState } from '../SessionMonitor';
import { messageOf, queryForTenant } from '../scope/operatorShared';
import { TypedFilterBuilder } from '../TypedFilterBuilder';
import {
  emptyTypedFilterAst, filteredRequestRefreshDelay, mergeLiveRequestEvents, mergeRefreshedRequestPage,
  summarizeVisibleRequests, typedFiltersActive, typedRequestQueryBody,
} from '../traffic/requestTraffic';

export function RequestsPage({ token, tenant, liveEvents, streamRevision, streamState, streamError, onOpenSessions, onOpenSession, requestFocus, onRequestFocusHandled, requestDrilldown, onRequestDrilldownHandled }: {
  token: string;
  tenant: string;
  liveEvents: ReadonlyMap<string, RequestEvent>;
  streamRevision: number;
  streamState: SessionStreamState;
  streamError: string;
  onOpenSessions: () => void;
  onOpenSession: (sessionId: string) => void;
  requestFocus?: { requestId: string; revision: number };
  onRequestFocusHandled?: (revision: number) => void;
  requestDrilldown?: { ast: TypedFilterAst; revision: number };
  onRequestDrilldownHandled?: (revision: number) => void;
}) {
  const { t } = useI18n();
  const [requests, setRequests] = useState<RequestView[]>([]);
  const [upstreams, setUpstreams] = useState<UpstreamAccount[]>([]);
  const [filters, setFilters] = useState<TypedFilterAst>(emptyTypedFilterAst);
  const [loading, setLoading] = useState(false);
  const [hasOlder, setHasOlder] = useState(false);
  const [detail, setDetail] = useState<RequestDetail>();
  const [error, setError] = useState('');
  const [upstreamError, setUpstreamError] = useState('');
  const [olderFilteredResultsStale, setOlderFilteredResultsStale] = useState(false);
  const sequence = useRef(0);
  const errorSource = useRef<'detail' | 'load' | 'refresh'>();
  const olderFilteredResultsVisible = useRef(false);
  const requestsRef = useRef(requests);
  const loadingRef = useRef(false);
  const filteredRefresh = useRef<{
    controller?: AbortController;
    firstPendingAt?: number;
    inFlight: boolean;
    pending: boolean;
    requestSequence: number;
    scopeSequence: number;
    timer?: number;
  }>({ inFlight: false, pending: false, requestSequence: 0, scopeSequence: 0 });
  const loadAbort = useRef<AbortController | null>(null);
  const upstreamSequence = useRef(0);
  const detailSequence = useRef(0);
  const detailAbort = useRef<AbortController | null>(null);
  // The initial snapshot and the live stream resolve independently. Keep the
  // latest event map available to an in-flight snapshot so an event received
  // before the snapshot completes cannot be overwritten by that stale result.
  const liveEventsRef = useRef(liveEvents);
  liveEventsRef.current = liveEvents;
  const hasOlderRef = useRef(hasOlder);
  const scope = useRef({ token, tenant, filters });
  hasOlderRef.current = hasOlder;
  loadingRef.current = loading;
  requestsRef.current = requests;
  scope.current = { token, tenant, filters };

  function cancelFilteredRefresh() {
    const state = filteredRefresh.current;
    state.scopeSequence += 1;
    state.requestSequence += 1;
    if (state.timer !== undefined) window.clearTimeout(state.timer);
    state.timer = undefined;
    state.controller?.abort();
    state.controller = undefined;
    state.firstPendingAt = undefined;
    state.inFlight = false;
    state.pending = false;
  }

  function scheduleFilteredRefresh() {
    const state = filteredRefresh.current;
    const currentScope = scope.current;
    if (!currentScope.token || !currentScope.tenant || !typedFiltersActive(currentScope.filters)) return;
    state.pending = true;
    if (state.inFlight || loadingRef.current) return;

    const now = Date.now();
    state.firstPendingAt ??= now;
    if (state.timer !== undefined) window.clearTimeout(state.timer);
    const scopeSequence = state.scopeSequence;
    state.timer = window.setTimeout(() => {
      state.timer = undefined;
      if (filteredRefresh.current.scopeSequence === scopeSequence) void refreshFilteredRequests();
    }, filteredRequestRefreshDelay(now, state.firstPendingAt));
  }

  async function refreshFilteredRequests() {
    const state = filteredRefresh.current;
    if (state.inFlight || loadingRef.current || !state.pending) return;
    const currentScope = scope.current;
    if (!currentScope.token || !currentScope.tenant || !typedFiltersActive(currentScope.filters)) return;
    state.inFlight = true;
    state.pending = false;
    state.firstPendingAt = undefined;
    const requestSequence = ++state.requestSequence;
    const scopeSequence = state.scopeSequence;
    const controller = new AbortController();
    state.controller = controller;
    try {
      const next = await api<RequestListResponse>('/internal/v1/requests/query', currentScope.token, {
        method: 'POST', body: JSON.stringify(typedRequestQueryBody(currentScope.tenant, currentScope.filters)), signal: controller.signal,
      });
      const latest = scope.current;
      if (controller.signal.aborted || state.requestSequence !== requestSequence || state.scopeSequence !== scopeSequence
        || latest.token !== currentScope.token || latest.tenant !== currentScope.tenant || latest.filters !== currentScope.filters) return;
      // Keep the current drawer and scroll position intact. This replacement is
      // the exact server-filtered first page, not a guessed local merge.
      setRequests((current) => mergeRefreshedRequestPage(current, next, olderFilteredResultsVisible.current));
      // A retained history tail owns its pagination state. A first-page
      // cursor must not reopen an already exhausted tail.
      if (!olderFilteredResultsVisible.current || next.next_cursor === null) {
        setHasOlder(next.next_cursor !== null);
      }
      if (next.next_cursor === null) {
        olderFilteredResultsVisible.current = false;
        setOlderFilteredResultsStale(false);
      }
      if (errorSource.current === 'refresh') {
        errorSource.current = undefined;
        setError('');
      }
    } catch (reason) {
      if (!controller.signal.aborted && state.requestSequence === requestSequence && state.scopeSequence === scopeSequence) {
        errorSource.current = 'refresh';
        setError(messageOf(reason, t('common.requestFailed')));
      }
    } finally {
      if (state.requestSequence !== requestSequence || state.scopeSequence !== scopeSequence) return;
      state.controller = undefined;
      state.inFlight = false;
      if (state.pending) scheduleFilteredRefresh();
    }
  }

  async function load(nextFilters: TypedFilterAst, older = false) {
    if (!token || !tenant) return;
    const last = requests.at(-1);
    const before = last ? { before_created_at: last.created_at, before_id: last.request_id } : undefined;
    if (older && (!hasOlder || !before)) return;
    const refreshWasActive = older && (filteredRefresh.current.pending || filteredRefresh.current.inFlight);
    // A foreground page request owns the request list until it settles. Abort
    // any background first-page refresh rather than running two query POSTs.
    cancelFilteredRefresh();
    loadAbort.current?.abort();
    const controller = new AbortController();
    loadAbort.current = controller;
    const request = ++sequence.current;
    const currentScope = { token, tenant, filters: nextFilters };
    if (!older) {
      olderFilteredResultsVisible.current = false;
      setOlderFilteredResultsStale(false);
      setRequests([]); setHasOlder(false); setDetail(undefined);
    }
    loadingRef.current = true;
    errorSource.current = undefined;
    setLoading(true); setError('');
    try {
      const next = await api<RequestListResponse>('/internal/v1/requests/query', token, {
        method: 'POST', body: JSON.stringify(typedRequestQueryBody(tenant, nextFilters, older ? before : undefined)), signal: controller.signal,
      });
      const latest = scope.current;
      if (controller.signal.aborted || request !== sequence.current || latest.token !== currentScope.token || latest.tenant !== currentScope.tenant || latest.filters !== currentScope.filters) return;
      setRequests((current) => older
        ? [...current, ...next.requests.filter((value) => !current.some((existing) => existing.request_id === value.request_id))]
        : typedFiltersActive(nextFilters) ? next.requests : mergeLiveRequestEvents(next.requests, new Map(liveEventsRef.current), next.next_cursor === null));
      setHasOlder(next.next_cursor !== null);
      if (older && typedFiltersActive(nextFilters) && next.requests.length > 0) olderFilteredResultsVisible.current = true;
    } catch (reason) {
      if (request === sequence.current && !controller.signal.aborted) {
        if (!older) { setRequests([]); setHasOlder(false); }
        errorSource.current = 'load';
        setError(messageOf(reason, t('common.requestFailed')));
      }
    } finally {
      if (request === sequence.current && loadAbort.current === controller) {
        loadAbort.current = null;
        loadingRef.current = false;
        setLoading(false);
        if (refreshWasActive) filteredRefresh.current.pending = true;
        if (filteredRefresh.current.pending) scheduleFilteredRefresh();
      }
    }
  }

  useEffect(() => {
    cancelFilteredRefresh();
    loadAbort.current?.abort(); loadAbort.current = null;
    olderFilteredResultsVisible.current = false;
    errorSource.current = undefined;
    loadingRef.current = false;
    sequence.current += 1; setFilters(emptyTypedFilterAst); setRequests([]); setDetail(undefined); setHasOlder(false); setLoading(false); setOlderFilteredResultsStale(false); setError(''); setUpstreamError('');
    if (!token || !tenant) { setUpstreams([]); return () => { cancelFilteredRefresh(); loadAbort.current?.abort(); }; }
    const upstreamRequest = ++upstreamSequence.current;
    void api<UpstreamAccount[]>(`/internal/v1/upstreams${queryForTenant(tenant)}`, token)
      .then((values) => { if (upstreamRequest === upstreamSequence.current) { setUpstreams(values); setUpstreamError(''); } })
      .catch((reason) => { if (upstreamRequest === upstreamSequence.current) { setUpstreams([]); setUpstreamError(messageOf(reason, t('common.requestFailed'))); } });
    void load(emptyTypedFilterAst);
    return () => { cancelFilteredRefresh(); loadAbort.current?.abort(); };
  }, [tenant, token]);

  useEffect(() => {
    detailSequence.current += 1; detailAbort.current?.abort(); detailAbort.current = null; setDetail(undefined);
    return () => { detailSequence.current += 1; detailAbort.current?.abort(); };
  }, [tenant, token]);

  useEffect(() => {
    if (liveEvents.size === 0) return;
    if (typedFiltersActive(filters)) {
      const terminalizedVisiblePending = olderFilteredResultsVisible.current
        && [...liveEventsRef.current.values()].some((event) => event.event_kind === 'finished'
          && requestsRef.current.some((request) => request.request_id === event.request_id && request.status_code === null));
      if (terminalizedVisiblePending) setOlderFilteredResultsStale(true);
      scheduleFilteredRefresh();
      return;
    }
    setRequests((current) => mergeLiveRequestEvents(current, new Map(liveEventsRef.current), !hasOlderRef.current));
  }, [streamRevision]);

  useEffect(() => {
    if (!loading && filteredRefresh.current.pending) scheduleFilteredRefresh();
  }, [loading]);

  async function openRequestDetail(requestId: string) {
    const requestSequence = ++detailSequence.current;
    detailAbort.current?.abort(); const controller = new AbortController(); detailAbort.current = controller;
    try {
      errorSource.current = undefined;
      setError('');
      const next = await api<RequestDetail>(`/internal/v1/requests/${requestId}${queryForTenant(tenant)}`, token, { signal: controller.signal });
      if (requestSequence === detailSequence.current) setDetail(next);
    } catch (reason) {
      if (requestSequence === detailSequence.current && !controller.signal.aborted) {
        errorSource.current = 'detail';
        setError(messageOf(reason, t('traffic.detailFailed')));
      }
    } finally { if (detailAbort.current === controller) detailAbort.current = null; }
  }

  async function selectRequest(request: RequestView) {
    await openRequestDetail(request.request_id);
  }

  useEffect(() => {
    if (!requestFocus) return;
    void openRequestDetail(requestFocus.requestId);
    onRequestFocusHandled?.(requestFocus.revision);
  }, [requestFocus?.revision, tenant, token]);

  useEffect(() => {
    if (!requestDrilldown) return;
    cancelFilteredRefresh();
    setFilters(requestDrilldown.ast);
    scope.current = { token, tenant, filters: requestDrilldown.ast };
    void load(requestDrilldown.ast);
    onRequestDrilldownHandled?.(requestDrilldown.revision);
  }, [requestDrilldown?.revision, tenant, token]);

  return <>
    {error && <div className="notice error" role="alert">{error}</div>}
    {upstreamError && <div className="notice error" role="alert">{upstreamError}</div>}
    {streamError && <div className="notice error" role="alert">{streamError}</div>}
    <RequestsPanel requests={requests} upstreams={upstreams} filters={filters} loading={loading} hasOlder={hasOlder} streamState={streamState} token={token} tenant={tenant} olderFilteredResultsStale={olderFilteredResultsStale}
      onApply={(next) => { setFilters(next); scope.current = { token, tenant, filters: next }; void load(next); }}
      onClear={() => { setFilters(emptyTypedFilterAst); scope.current = { token, tenant, filters: emptyTypedFilterAst }; void load(emptyTypedFilterAst); }}
      onLoadOlder={() => void load(filters, true)} onRefreshFilteredResults={() => void load(filters)} onSelect={selectRequest} onOpenSessions={onOpenSessions} onOpenSession={onOpenSession} />
    {detail && <RequestDrawer detail={detail} onOpenSession={onOpenSession} onClose={() => setDetail(undefined)} />}
  </>;
}

function RequestsPanel({ requests, upstreams, filters, loading, hasOlder, streamState, token, tenant, olderFilteredResultsStale, onApply, onClear, onLoadOlder, onRefreshFilteredResults, onSelect, onOpenSessions, onOpenSession }: {
  requests: RequestView[];
  upstreams: UpstreamAccount[];
  filters: TypedFilterAst;
  loading: boolean;
  hasOlder: boolean;
  streamState: SessionStreamState;
  token: string;
  tenant: string;
  olderFilteredResultsStale: boolean;
  onApply: (filters: TypedFilterAst) => void;
  onClear: () => void;
  onLoadOlder: () => void;
  onRefreshFilteredResults: () => void;
  onSelect: (request: RequestView) => Promise<void>;
  onOpenSessions: () => void;
  onOpenSession: (sessionId: string) => void;
}) {
  const { locale, t } = useI18n();
  const summary = summarizeVisibleRequests(requests);
  return <article className="panel"><div className="panel-title traffic-heading"><div><h2>{typedFiltersActive(filters) ? t('traffic.filtered') : t('traffic.live')}</h2><span>{typedFiltersActive(filters) ? t('traffic.filteredHint') : t('traffic.liveHint')}</span></div><div className="traffic-heading-actions"><div className={`request-live-state session-live-state ${streamState}`} role="status">{t(`sessions.live.${streamState}`)}</div><div className="segmented" role="group" aria-label={t('sessions.monitorMode')}><button type="button" className="active" aria-pressed="true">{t('sessions.requestsMode')}</button><button type="button" aria-pressed="false" onClick={onOpenSessions}>{t('sessions.sessionsMode')}</button></div></div></div>
    <TypedFilterBuilder ast={filters} disabled={loading} onApply={onApply} onClear={onClear} scope="requests" token={token} tenant={tenant} upstreams={upstreams} />
    {olderFilteredResultsStale && <div className="notice warning" role="status">{t('traffic.olderFilteredResultsStale')}<button type="button" className="secondary" disabled={loading} onClick={onRefreshFilteredResults}>{t('traffic.refreshFilteredResults')}</button></div>}
    {requests.length > 0 && <section className="metrics request-traffic-metrics" aria-label={t('monitoring.summary')}>
      <NumberMetric label={t('usage.requests')} value={summary.requests} />
      <NumberMetric label={t('traffic.success')} value={summary.successful} tone="positive" />
      <NumberMetric label={t('traffic.failure')} value={summary.failed} tone="negative" />
      <NumberMetric label={t('common.running')} value={summary.running} tone={summary.running > 0 ? 'pending' : undefined} />
      <Metric label={t('usage.successRate')} value={formatPercent(summary.successRate, locale)} tone="positive" />
      <Metric label={t('usage.average')} value={formatMilliseconds(summary.averageDurationMs, locale)} />
    </section>}
    <RequestTable requests={requests} onSelect={(request) => void onSelect(request)} onOpenSession={onOpenSession} />
    {hasOlder && <div className="load-more"><button type="button" className="secondary" disabled={loading} onClick={onLoadOlder}>{loading ? t('common.loading') : t('traffic.loadOlder')}</button></div>}
  </article>;
}

function RequestDrawer({ detail, onOpenSession, onClose }: { detail: RequestDetail; onOpenSession: (sessionId: string) => void; onClose: () => void }) {
  const { t } = useI18n();
  return <DrawerFrame title={detail.model} eyebrow={t('request.operatorDiagnosis')} onClose={onClose}>
    <RequestDiagnostics request={detail} onOpenSession={onOpenSession} />
    <div className="request-diagnostics request-archive-diagnostics">
      <span><b>{t('self.archive')}</b>{detail.archive_complete ? t('request.archiveComplete') : t('request.archiveIncomplete')}</span>
      {detail.provenance && <span><b>{t('request.provenance')}</b>{detail.provenance.unlinked ? t('request.archiveOnly') : t('request.exactArchive')} · {detail.provenance.source}</span>}
    </div>
    <details className="request-technical-details">
      <summary>{t('request.technicalDetails')}</summary>
      <h3>{t('request.request')}</h3><pre>{JSON.stringify(detail.request_body, null, 2)}</pre>
      <h3>{t('request.response')}</h3><pre>{JSON.stringify(detail.response_body, null, 2)}</pre>
      {detail.provenance && <><h3>{t('request.provenance')}</h3><pre>{JSON.stringify(detail.provenance, null, 2)}</pre></>}
    </details>
  </DrawerFrame>;
}
