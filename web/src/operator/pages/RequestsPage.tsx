import { useEffect, useId, useMemo, useRef, useState } from 'react';
import { Button, ToggleButton } from '@fluentui/react-components';
import { DetailTooltip, LoadingProgress, LoadingState, PageLoadingRegion } from '../../design-system';
import { api, apiDiagnosticMessage } from '../../api';
import { DrawerFrame, RequestDiagnostics, RequestTable } from '../../components';
import { formatCurrencyDisplay, formatDurationDisplay, formatMetricDisplay, formatPercent } from '../../format';
import { LocalSettlementNotice, localSettlementLabel } from '../../LocalSettlementNotice';
import { AnalyticsMetric } from '../AnalyticsMetric';
import { ImageGenerationQuarantine } from '../ImageGenerationQuarantine';
import { displayTimeZone } from '../../charts/displayTimeZone';
import { useI18n } from '../../i18n';
import { useSessionParentLabel } from '../sessionTasks';
import { unnamedSessionName } from '../../sessionTitles';
import { credentialDisplayName } from '../../identityPresentation';
import type { RequestDetail, RequestEvent, RequestListResponse, RequestView, TypedFilterAst, UpstreamAccount } from '../../types';
import { RequestEgressDetails } from '../RequestEgressDetails';
import type { SessionStreamState } from '../SessionMonitor';
import { restoreRequestSessionOrigin, type SessionRequestOrigin } from '../sessionNavigation';
import { queryForTenant } from '../scope/operatorShared';
import { TypedFilterBuilder } from '../TypedFilterBuilder';
import { RequestWorkspaceTabs } from '../RequestWorkspaceTabs';
import { RequestRefreshControl } from '../traffic/RequestRefreshControl';
import { mergeBatchedRequestPage } from '../traffic/requestRefresh';
import { RequestOverflowReconciliation } from '../traffic/requestOverflowReconciliation';
import {
  emptyTypedFilterAst, mergeRefreshedRequestPage, mergeSessionContext, requestViewFromEvent,
  summarizeVisibleRequests, typedFiltersActive, typedRequestQueryBody, visibleRequestMetricSeries,
} from '../traffic/requestTraffic';

export function RequestsPage({ token, tenant, writeTenant = tenant, liveEvents, streamRevision, streamState, streamError, onOpenSessions, onOpenSession, onOpenGenerations, requestFocus, onRequestFocusHandled, requestDrilldown, onRequestDrilldownHandled, requestRefresh, streamOverflowRevision = 0, onProtectRequests }: {
  token: string;
  tenant: string;
  writeTenant?: string;
  liveEvents: ReadonlyMap<string, RequestEvent>;
  streamRevision: number;
  streamState: SessionStreamState;
  streamError: string;
  onOpenSessions: () => void;
  onOpenSession: (sessionId: string, keyId?: string, origin?: SessionRequestOrigin) => void;
  onOpenGenerations?: () => void;
  requestFocus?: { requestId: string; revision: number; restore?: { scrollY: number; openDetail: boolean } };
  onRequestFocusHandled?: (revision: number) => void;
  requestDrilldown?: { ast: TypedFilterAst; revision: number };
  onRequestDrilldownHandled?: (revision: number) => void;
  requestRefresh?: { intervalMs: number; paused: boolean; onIntervalChange: (value: number) => void };
  streamOverflowRevision?: number;
  onProtectRequests?: (ids: string[], consumerPaused?: boolean) => void;
}) {
  const { t } = useI18n();
  const diagnosticLabels = { requestId: t('request.correlationId'), streamInterrupted: t('request.streamInterrupted') };
  const [requests, setRequests] = useState<RequestView[]>([]);
  const [snapshotRevision, setSnapshotRevision] = useState(0);
  const [upstreams, setUpstreams] = useState<UpstreamAccount[]>([]);
  const [filters, setFilters] = useState<TypedFilterAst>(emptyTypedFilterAst);
  const [loading, setLoading] = useState(false);
  const [hasOlder, setHasOlder] = useState(false);
  const [detailResult, setDetail] = useState<{ value: RequestDetail; token: string; tenant: string; requestId: string }>();
  const [error, setError] = useState('');
  const [upstreamError, setUpstreamError] = useState('');
  const [filteredResultsStale, setFilteredResultsStale] = useState(false);
  const [imageReviewOpen, setImageReviewOpen] = useState(false);
  const sequence = useRef(0);
  const errorSource = useRef<'detail' | 'load' | 'refresh' | undefined>(undefined);
  const loadedHistoryIds = useRef(new Set<string>());
  const requestsRef = useRef(requests);
  const loadingRef = useRef(false);
  const paused = useRef(false);
  // The pause tier is page-local: SSE and the bounded hook buffer keep running
  // while the visible list, metrics and drawer stay frozen until resume.
  const [userPaused, setUserPaused] = useState(false);
  const streamPaused = userPaused || (requestRefresh?.paused ?? false);
  paused.current = streamPaused;
  const previousOverflowRevision = useRef(streamOverflowRevision);
  const overflowController = useRef<AbortController | null>(null);
  const startOverflowRefresh = useRef<(ticket: number) => void>(() => undefined);
  const overflowRefresh = useRef<RequestOverflowReconciliation | undefined>(undefined);
  if (!overflowRefresh.current) {
    overflowRefresh.current = new RequestOverflowReconciliation({
      now: () => Date.now(),
      schedule: (callback, delay) => window.setTimeout(callback, delay),
      cancel: timer => window.clearTimeout(timer),
    }, ticket => startOverflowRefresh.current(ticket));
  }
  const loadAbort = useRef<AbortController | null>(null);
  const operationalAbort = useRef<AbortController | null>(null);
  const reconciledOperationalEvents = useRef(new Map<string, string>());
  const upstreamSequence = useRef(0);
  const detailSequence = useRef(0);
  const detailAbort = useRef<AbortController | null>(null);
  const selectedRequestId = useRef<string | undefined>(undefined);
  const pendingRequestReturn = useRef<SessionRequestOrigin | undefined>(undefined);
  // State mirror of the open detail so the protection effect reruns on
  // selection changes; a ref alone would not trigger React.
  const [selectedRequest, setSelectedRequest] = useState<string | undefined>(undefined);
  const refreshedTerminalEvent = useRef<string | undefined>(undefined);
  // Gate during render: effects must not expose a previous credential/tenant's
  // detail for even the first commit after a scope change.
  const detail = detailResult?.token === token && detailResult.tenant === tenant
    && detailResult.requestId === selectedRequestId.current ? detailResult.value : undefined;
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

  function cancelOverflowRefresh(preserveDirty = false, resetCooldown = false) {
    overflowController.current?.abort();
    overflowController.current = null;
    overflowRefresh.current?.reset(preserveDirty, resetCooldown);
  }

  function syncOverflowRefreshBlocked() {
    const currentScope = scope.current;
    overflowRefresh.current?.setBlocked(
      !currentScope.token || !currentScope.tenant || typedFiltersActive(currentScope.filters)
      || loadingRef.current || paused.current,
    );
  }

  async function refreshOverflowFirstPage(ticket: number) {
    const currentScope = scope.current;
    if (!currentScope.token || !currentScope.tenant || typedFiltersActive(currentScope.filters)
      || loadingRef.current || paused.current) {
      overflowRefresh.current?.defer(ticket);
      return;
    }
    const controller = new AbortController();
    overflowController.current = controller;
    let succeeded = false;
    try {
      const next = await api<RequestListResponse>('/internal/v1/requests/query', currentScope.token, {
        method: 'POST', body: JSON.stringify(typedRequestQueryBody(currentScope.tenant, currentScope.filters)), signal: controller.signal,
      });
      const latest = scope.current;
      if (controller.signal.aborted || latest.token !== currentScope.token || latest.tenant !== currentScope.tenant
        || latest.filters !== currentScope.filters || typedFiltersActive(latest.filters)) {
        syncOverflowRefreshBlocked();
        return;
      }
      // Replace the live window with the authoritative server first page, then
      // re-apply the bounded live buffer. Never runs for filtered views.
      const current = requestsRef.current;
      const page = mergeBatchedRequestPage(mergeRefreshedRequestPage(current, next, current.length > 100), new Map(liveEventsRef.current), next.next_cursor !== null, loadedHistoryIds.current);
      setRequests(page.requests); setHasOlder(page.hasOlder);
      setSnapshotRevision(value => value + 1);
      succeeded = true;
      if (errorSource.current === 'refresh') {
        errorSource.current = undefined;
        setError('');
      }
    } catch (reason) {
      if (!controller.signal.aborted) {
        errorSource.current = 'refresh';
        setError(apiDiagnosticMessage(reason, t('common.requestFailed'), diagnosticLabels));
      }
    } finally {
      if (overflowController.current === controller) overflowController.current = null;
      overflowRefresh.current?.finish(ticket, succeeded);
    }
  }
  startOverflowRefresh.current = ticket => { void refreshOverflowFirstPage(ticket); };

  async function load(nextFilters: TypedFilterAst, older = false) {
    if (!token || !tenant) return;
    const last = requests.at(-1);
    const before = last ? { before_created_at: last.created_at, before_id: last.request_id } : undefined;
    if (older && (!hasOlder || !before)) return;
    // Freeze insertion before the page fetch, not after it resolves: otherwise
    // a live batch could move the visible tail while this cursor is in flight.
    if (older) for (const request of requestsRef.current) loadedHistoryIds.current.add(request.request_id);
    const refreshWasActive = older && (overflowRefresh.current?.needsReconcile ?? false);
    // A foreground page request owns the request list until it settles. Abort
    // any background first-page refresh rather than running two query POSTs.
    // Its dirty edge remains sticky: a just-claimed timer must not be mistaken
    // for a completed authoritative refresh while the foreground lane owns it.
    overflowRefresh.current?.setBlocked(true);
    cancelOverflowRefresh(true);
    operationalAbort.current?.abort();
    reconciledOperationalEvents.current.clear();
    loadAbort.current?.abort();
    const controller = new AbortController();
    loadAbort.current = controller;
    const request = ++sequence.current;
    const currentScope = { token, tenant, filters: nextFilters };
    if (!older) {
      loadedHistoryIds.current.clear();
      setFilteredResultsStale(false);
      setRequests([]); setHasOlder(false); closeRequestDetail();
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
      if (!older && !typedFiltersActive(nextFilters)) {
        const page = mergeBatchedRequestPage(next.requests, new Map(liveEventsRef.current), next.next_cursor !== null);
        setRequests(page.requests); setHasOlder(page.hasOlder);
      } else {
        if (older) for (const request of next.requests) loadedHistoryIds.current.add(request.request_id);
        setRequests((current) => older
          ? [...current, ...next.requests.filter((value) => !current.some((existing) => existing.request_id === value.request_id))]
          : next.requests);
        setHasOlder(next.next_cursor !== null);
      }
      setSnapshotRevision(value => value + 1);
    } catch (reason) {
      if (request === sequence.current && !controller.signal.aborted) {
        if (!older) { setRequests([]); setHasOlder(false); }
        errorSource.current = 'load';
        setError(apiDiagnosticMessage(reason, t('common.requestFailed'), diagnosticLabels));
      }
    } finally {
      if (request === sequence.current && loadAbort.current === controller) {
        loadAbort.current = null;
        loadingRef.current = false;
        setLoading(false);
        syncOverflowRefreshBlocked();
        // Pagination owns the network lane while it is active, but it cannot
        // consume an already-dirty first-page overflow. Re-arm exactly one
        // reconciliation after the older page settles.
        if (refreshWasActive) overflowRefresh.current?.signal();
      }
    }
  }

  useEffect(() => {
    // Reset every scope-bound cursor/buffer ref before any reload. Syncing the
    // overflow baseline to the current prop cannot swallow a new-scope
    // overflow: the stream's overflowRevision only ever increments, and the
    // new scope's batch is recreated empty after this commit, so any future
    // overflow still arrives as a strictly larger revision.
    loadedHistoryIds.current.clear();
    reconciledOperationalEvents.current.clear();
    previousOverflowRevision.current = streamOverflowRevision;
    onProtectRequests?.([]);
    cancelOverflowRefresh(false, true);
    loadAbort.current?.abort(); loadAbort.current = null;
    errorSource.current = undefined;
    loadingRef.current = false;
    sequence.current += 1; setFilters(emptyTypedFilterAst); setRequests([]); setDetail(undefined); setHasOlder(false); setLoading(false); setFilteredResultsStale(false); setImageReviewOpen(false); setError(''); setUpstreamError('');
    if (!token || !tenant) { setUpstreams([]); return () => { cancelOverflowRefresh(); loadAbort.current?.abort(); }; }
    const upstreamRequest = ++upstreamSequence.current;
    void api<UpstreamAccount[]>(`/internal/v1/upstreams${queryForTenant(tenant)}`, token)
      .then((values) => { if (upstreamRequest === upstreamSequence.current) { setUpstreams(values); setUpstreamError(''); } })
      .catch((reason) => { if (upstreamRequest === upstreamSequence.current) { setUpstreams([]); setUpstreamError(apiDiagnosticMessage(reason, t('common.requestFailed'), diagnosticLabels)); } });
    void load(emptyTypedFilterAst);
    return () => { cancelOverflowRefresh(); loadAbort.current?.abort(); };
  }, [tenant, token, writeTenant]);

  useEffect(() => {
    selectedRequestId.current = undefined; refreshedTerminalEvent.current = undefined;
    setSelectedRequest(undefined);
    detailSequence.current += 1; detailAbort.current?.abort(); detailAbort.current = null; setDetail(undefined);
    return () => { detailSequence.current += 1; detailAbort.current?.abort(); };
  }, [tenant, token]);

  useEffect(() => {
    // Report the pause tier together with the protected ids: while paused the
    // hook batch stops publishing revisions entirely but keeps buffering SSE.
    // The open detail stays protected even after live traffic pushes it out
    // of the rendered window.
    const visibleIds = requests.map(request => request.request_id);
    onProtectRequests?.(selectedRequest && !visibleIds.includes(selectedRequest) ? [...visibleIds, selectedRequest] : visibleIds, userPaused);
  }, [requests, selectedRequest, userPaused, onProtectRequests]);
  useEffect(() => () => onProtectRequests?.([]), [onProtectRequests]);

  useEffect(() => {
    if (streamPaused) {
      syncOverflowRefreshBlocked();
      cancelOverflowRefresh(true);
    } else {
      syncOverflowRefreshBlocked();
    }
  }, [streamPaused]);

  useEffect(() => {
    if (streamOverflowRevision !== previousOverflowRevision.current) {
      previousOverflowRevision.current = streamOverflowRevision;
      // Overflow reconciliation serves only the unfiltered live first page.
      // Filtered and history views stay stable until an explicit refresh.
      if (typedFiltersActive(filters) || loadedHistoryIds.current.size) return;
      overflowRefresh.current?.signal();
    }
  }, [streamOverflowRevision]);

  useEffect(() => {
    // SSE batches never auto-refresh a filtered view. They only surface the
    // stale hint so the user can explicitly reload filtered history.
    if (streamPaused || (loadingRef.current && requestsRef.current.length === 0) || liveEvents.size === 0) return;
    if (typedFiltersActive(filters)) {
      // Any published batch marks filtered results stale: the list itself
      // stays frozen until the user explicitly refreshes it.
      setFilteredResultsStale(true);
      return;
    }
    const page = mergeBatchedRequestPage(requestsRef.current, new Map(liveEventsRef.current), hasOlderRef.current, loadedHistoryIds.current);
    setRequests(page.requests); setHasOlder(page.hasOlder);
  }, [streamRevision, streamPaused]);

  useEffect(() => {
    syncOverflowRefreshBlocked();
  }, [filters, loading, streamPaused]);

  useEffect(() => {
    if (!token || !tenant || loadingRef.current || streamPaused || typedFiltersActive(filters)) return;
    const controller = new AbortController();
    operationalAbort.current = controller;
    const currentScope = { token, tenant, filters };
    const requestSequence = sequence.current;
    const visibleIds = new Set(requestsRef.current.map(request => request.request_id));
    for (const requestId of reconciledOperationalEvents.current.keys()) {
      if (!visibleIds.has(requestId)) reconciledOperationalEvents.current.delete(requestId);
    }
    for (const request of requestsRef.current) {
      const event = liveEventsRef.current.get(request.request_id);
      const completedAt = event?.completed_at;
      const retainedTerminal = typeof completedAt === 'number' && Number.isFinite(completedAt) && completedAt >= request.created_at;
      if (!request.operational_terminal || request.operational_terminal.financial_state === 'applied'
        || !event || (event.event_kind !== 'finished' && event.event_kind !== 'projected' && !retainedTerminal)
        || reconciledOperationalEvents.current.get(request.request_id) === event.event_id) continue;
      void api<RequestDetail>(`/internal/v1/requests/${request.request_id}${queryForTenant(tenant)}`, token, { signal: controller.signal })
        .then(next => {
          const latest = scope.current;
          if (controller.signal.aborted || paused.current || loadingRef.current || sequence.current !== requestSequence
            || latest.token !== currentScope.token || latest.tenant !== currentScope.tenant || latest.filters !== currentScope.filters
            || next.request_id !== request.request_id || !next.operational_terminal) return;
          reconciledOperationalEvents.current.set(request.request_id, event.event_id);
          setRequests(current => current.map(row => {
            if (row.request_id !== request.request_id || row.operational_terminal?.financial_state === 'applied') return row;
            const hydrated = { ...next, egress: next.egress ?? row.egress,
              session_context: mergeSessionContext(row.session_context, next) };
            const retained = liveEventsRef.current.get(row.request_id);
            return retained ? requestViewFromEvent(retained, hydrated) ?? hydrated : hydrated;
          }));
        })
        .catch(reason => {
          if (controller.signal.aborted) return;
          errorSource.current = 'refresh';
          setError(apiDiagnosticMessage(reason, t('common.requestFailed'), diagnosticLabels));
        });
    }
    return () => {
      controller.abort();
      if (operationalAbort.current === controller) operationalAbort.current = null;
    };
  }, [streamRevision, snapshotRevision, streamPaused, filters, loading, token, tenant, writeTenant]);

  async function openRequestDetail(requestId: string) {
    if (selectedRequestId.current !== requestId) setDetail(undefined);
    selectedRequestId.current = requestId;
    setSelectedRequest(requestId);
    const requestSequence = ++detailSequence.current;
    detailAbort.current?.abort(); const controller = new AbortController(); detailAbort.current = controller;
    try {
      errorSource.current = undefined;
      setError('');
      const next = await api<RequestDetail>(`/internal/v1/requests/${requestId}${queryForTenant(tenant)}`, token, { signal: controller.signal });
      if (requestSequence === detailSequence.current && selectedRequestId.current === requestId && !controller.signal.aborted) {
        const row = requestsRef.current.find(request => request.request_id === requestId);
        const previous = detailResult?.token === token && detailResult.tenant === tenant
          && detailResult.requestId === requestId ? detailResult.value : row;
        const hydrated = { ...next, egress: next.egress ?? previous?.egress,
          session_context: mergeSessionContext(previous?.session_context, next) };
        setDetail({ value: hydrated, token, tenant, requestId });
      }
    } catch (reason) {
      if (requestSequence === detailSequence.current && !controller.signal.aborted) {
        errorSource.current = 'detail';
        setError(apiDiagnosticMessage(reason, t('traffic.detailFailed'), diagnosticLabels));
      }
    } finally { if (detailAbort.current === controller) detailAbort.current = null; }
  }

  async function selectRequest(request: RequestView) {
    setDetail(undefined);
    await openRequestDetail(request.request_id);
  }

  function closeRequestDetail() {
    selectedRequestId.current = undefined;
    setSelectedRequest(undefined);
    detailSequence.current += 1;
    detailAbort.current?.abort(); detailAbort.current = null;
    setDetail(undefined);
  }

  useEffect(() => {
    if (streamPaused) return;
    const requestId = selectedRequestId.current;
    if (!requestId) return;
    const event = liveEvents.get(requestId);
    if (!event || (event.event_kind !== 'finished' && event.completed_at == null) || event.event_id === refreshedTerminalEvent.current) return;
    refreshedTerminalEvent.current = event.event_id;
    void openRequestDetail(requestId);
  }, [streamRevision, streamPaused]);

  useEffect(() => {
    if (!requestFocus) return;
    if (requestFocus.restore) {
      pendingRequestReturn.current = {
        route: 'requests', requestId: requestFocus.requestId,
        scrollY: requestFocus.restore.scrollY, openDetail: requestFocus.restore.openDetail,
      };
      if (!requestFocus.restore.openDetail) closeRequestDetail();
    } else {
      pendingRequestReturn.current = undefined;
      void openRequestDetail(requestFocus.requestId);
    }
    onRequestFocusHandled?.(requestFocus.revision);
  }, [requestFocus?.revision, tenant, token]);

  useEffect(() => {
    if (!requestDrilldown) return;
    cancelOverflowRefresh();
    setFilters(requestDrilldown.ast);
    scope.current = { token, tenant, filters: requestDrilldown.ast };
    void load(requestDrilldown.ast);
    onRequestDrilldownHandled?.(requestDrilldown.revision);
  }, [requestDrilldown?.revision, tenant, token]);

  useEffect(() => {
    const origin = pendingRequestReturn.current;
    const requestId = origin?.requestId;
    if (!origin || !requestId || !requests.some(request => request.request_id === requestId)) return;
    if (origin.openDetail && detail?.request_id !== origin.requestId) {
      if (selectedRequestId.current !== requestId) void openRequestDetail(requestId);
      return;
    }
    pendingRequestReturn.current = undefined;
    restoreRequestSessionOrigin(origin);
  }, [requests, detail?.request_id]);

  const openSession = (sessionId: string, keyId?: string, requestId?: string) => {
    onOpenSession(sessionId, keyId, {
      route: 'requests', requestId, filters, scrollY: window.scrollY,
      openDetail: Boolean(requestId && selectedRequestId.current === requestId && detail?.request_id === requestId),
    });
  };

  return <PageLoadingRegion scopeKey={`${token}\0${tenant}\0${writeTenant}\0requests`} label={t('common.loading')}>
    {error && <div className="notice error" role="alert">{t(errorSource.current === 'detail' ? 'request.detail' : 'request.listSource')}: {error}</div>}
    {upstreamError && <div className="notice error" role="alert">{t('request.upstreamSource')}: {upstreamError}</div>}
    {streamError && <div className="notice error" role="alert">{t('request.streamSource')}: {streamError}</div>}
    <RequestsPanel requests={requests} upstreams={upstreams} filters={filters} loading={loading} hasOlder={hasOlder} streamState={streamState} token={token} tenant={tenant} filteredResultsStale={filteredResultsStale} requestRefresh={requestRefresh} refreshPaused={userPaused} onToggleRefreshPaused={() => setUserPaused((value) => !value)} historyLoaded={loadedHistoryIds.current.size > 0} imageReviewOpen={imageReviewOpen} onToggleImageReview={() => setImageReviewOpen((open) => !open)}
      onApply={(next) => { setFilters(next); scope.current = { token, tenant, filters: next }; void load(next); }}
      onClear={() => { setFilters(emptyTypedFilterAst); scope.current = { token, tenant, filters: emptyTypedFilterAst }; void load(emptyTypedFilterAst); }}
      onLoadOlder={() => void load(filters, true)} onRefreshFilteredResults={() => void load(filters)} onSelect={selectRequest} onOpenSessions={onOpenSessions} onOpenSession={openSession} onOpenGenerations={onOpenGenerations} />
    {imageReviewOpen && <section className="request-image-review" aria-label={t('quarantine.title')}>
      <ImageGenerationQuarantine token={token} tenant={tenant} writeTenant={writeTenant} />
    </section>}
    {detail && <RequestDrawer token={token} tenant={tenant} detail={detail} upstreamName={upstreams.find((account) => account.id === detail.upstream_account_id)?.name} accountNames={new Map(upstreams.map(account => [account.id, account.name]))} onOpenSession={openSession} onClose={closeRequestDetail} />}
  </PageLoadingRegion>;
}

function RequestsPanel({ requests, upstreams, filters, loading, hasOlder, streamState, token, tenant, filteredResultsStale, onApply, onClear, onLoadOlder, onRefreshFilteredResults, onSelect, onOpenSessions, onOpenSession, onOpenGenerations, requestRefresh, refreshPaused = false, onToggleRefreshPaused, historyLoaded, imageReviewOpen, onToggleImageReview }: {
  requests: RequestView[];
  upstreams: UpstreamAccount[];
  filters: TypedFilterAst;
  loading: boolean;
  hasOlder: boolean;
  streamState: SessionStreamState;
  token: string;
  tenant: string;
  filteredResultsStale: boolean;
  historyLoaded: boolean;
  imageReviewOpen: boolean;
  onToggleImageReview: () => void;
  onApply: (filters: TypedFilterAst) => void;
  onClear: () => void;
  onLoadOlder: () => void;
  onRefreshFilteredResults: () => void;
  onSelect: (request: RequestView) => Promise<void>;
  onOpenSessions: () => void;
  onOpenSession: (sessionId: string, keyId?: string, requestId?: string) => void;
  onOpenGenerations?: () => void;
  requestRefresh?: { intervalMs: number; paused: boolean; onIntervalChange: (value: number) => void };
  refreshPaused?: boolean;
  onToggleRefreshPaused?: () => void;
}) {
  const { locale, t } = useI18n();
  const summary = summarizeVisibleRequests(requests);
  const averageDuration = formatDurationDisplay(summary.averageDurationMs, locale);
  const points = useMemo(() => visibleRequestMetricSeries(requests), [requests]);
  const sampling = { timestamps: points.map(point => point.timestamp), timeZone: displayTimeZone() };
  const count = (value: number) => formatMetricDisplay(value, locale);
  const settlementCurrency = summary.localCosts.length === 1 ? summary.localCosts[0].currency : undefined;
  return <article className="panel request-page-surface" aria-busy={loading}><div className="panel-title traffic-heading"><div><h2>{typedFiltersActive(filters) ? t('traffic.filtered') : t('traffic.live')}</h2><span>{typedFiltersActive(filters) ? t('traffic.filteredHint') : t('traffic.liveHint')}</span></div><div className="traffic-heading-actions"><Button appearance="secondary" aria-expanded={imageReviewOpen} onClick={onToggleImageReview}>{t('quarantine.menuItem')}</Button><div className={`request-live-state session-live-state ${streamState}`} role="status">{t(`sessions.live.${streamState}`)}</div><RequestWorkspaceTabs selected="requests" onRequests={() => undefined} onGenerations={onOpenGenerations ?? (() => undefined)} onSessions={onOpenSessions} /></div></div>
    {requestRefresh && <div className="request-refresh-row">
      <RequestRefreshControl intervalMs={requestRefresh.intervalMs} onIntervalChange={requestRefresh.onIntervalChange}
        paused={requestRefresh.paused || refreshPaused}
        pausedHint={refreshPaused && !requestRefresh.paused
          ? t('traffic.updatesPausedHint') : undefined} />
      {onToggleRefreshPaused && <span className="request-refresh-pause">
        <ToggleButton appearance="secondary" checked={refreshPaused} onClick={onToggleRefreshPaused}>{refreshPaused ? t('traffic.resumeUpdates') : t('traffic.pauseUpdates')}</ToggleButton>
      </span>}
    </div>}
    {historyLoaded && !typedFiltersActive(filters) && <div className="request-refresh-control"><span>{locale === 'zh-CN' ? '正在浏览历史：已显示请求继续更新，新请求暂不插入。' : 'Browsing history: visible requests keep updating; new requests are not inserted.'}</span><Button appearance="subtle" disabled={loading} onClick={onRefreshFilteredResults}>{locale === 'zh-CN' ? '返回最新请求' : 'Return to latest requests'}</Button></div>}
    <TypedFilterBuilder ast={filters} disabled={!token.trim()} onApply={onApply} onClear={onClear} scope="requests" token={token} tenant={tenant} upstreams={upstreams} />
    {filteredResultsStale && typedFiltersActive(filters) && <div className="notice warning" role="status">{t('traffic.filteredResultsStale')}<Button appearance="secondary" disabled={loading} onClick={onRefreshFilteredResults}>{t('traffic.refreshFilteredResults')}</Button></div>}
    {requests.length > 0 && <section className="metrics request-traffic-metrics" aria-label={t('monitoring.summary')}>
      <AnalyticsMetric {...sampling} label={t('usage.totalTokens')} value={count(summary.totalTokens).text} title={count(summary.totalTokens).title} trend={points.map(point => point.totalTokens)} />
      <AnalyticsMetric {...sampling} label={localSettlementLabel(locale)} labelContent={<LocalSettlementNotice />} value={summary.localCosts.length ? <span className="usage-cost-lines">{summary.localCosts.map(({ currency, cost }) => { const display = formatCurrencyDisplay(cost, currency, locale); return <span key={currency} title={display.title}>{display.text}</span>; })}</span> : '—'} formatSample={(_value, index) => { const cost = settlementCurrency ? points[index].localCosts.find(item => item.currency === settlementCurrency) : undefined; return cost ? formatCurrencyDisplay(cost.cost, cost.currency, locale).title ?? '—' : '—'; }} trend={settlementCurrency ? points.map((point) => { const cost = point.localCosts.find(item => item.currency === settlementCurrency); return cost ? cost.cost : null; }) : undefined} />
      <AnalyticsMetric {...sampling} label={t('usage.cacheRate')} labelContent={<DetailTooltip content={t('usage.cacheRateHint')}><span tabIndex={0}>{t('usage.cacheRate')}</span></DetailTooltip>} value={formatPercent(summary.cacheRate, locale)} ratio={summary.cacheRate} trend={points.map(point => point.cacheRate)} formatSample={value => formatPercent(value, locale)} />
      <AnalyticsMetric {...sampling} label={t('usage.average')} value={averageDuration.text} title={averageDuration.title} trend={points.map(point => point.averageDurationMs)} formatSample={value => { const display = formatDurationDisplay(value, locale); return display.title ?? display.text; }} />
    </section>}
    {requests.length > 0 && <p className="request-metrics-scope">{locale === 'zh-CN' ? '统计范围是当前已加载的请求。背景图按接收时间展示这些记录。' : 'These figures cover the requests loaded on this page. Background charts group them by reception time.'}</p>}
    <LoadingProgress active={loading && requests.length > 0} label={t('common.loading')} />
    {loading && requests.length === 0 ? <LoadingState label={t('common.loading')} level="page" /> : <RequestTable requests={requests} upstreamNames={new Map(upstreams.map((account) => [account.id, account.name]))} onSelect={(request) => void onSelect(request)} onOpenSession={onOpenSession} />}
    {hasOlder && <div className="load-more"><Button appearance="secondary" disabled={loading} onClick={onLoadOlder}>{loading ? t('common.loading') : t('traffic.loadOlder')}</Button></div>}
  </article>;
}

function RequestDrawer({ token, tenant, detail, upstreamName, accountNames, onOpenSession, onClose }: { token: string; tenant: string; detail: RequestDetail; upstreamName?: string; accountNames: ReadonlyMap<string, string>; onOpenSession: (sessionId: string, keyId?: string, requestId?: string) => void; onClose: () => void }) {
  const { locale, t } = useI18n();
  const [technicalOpen, setTechnicalOpen] = useState(false);
  const technicalId = useId();
  const parent = useSessionParentLabel(tenant, token, detail.credential_identity?.key_id, detail.session_context?.association === 'confirmed' ? detail.session_context.session_id : null, session => session.session_name?.trim() || unnamedSessionName(t, locale, session.last_activity_at, credentialDisplayName(session.key_alias, t), true));
  return <DrawerFrame title={detail.model} eyebrow={t('request.operatorDiagnosis')} onClose={onClose}>
    <RequestDiagnostics request={detail} onOpenSession={onOpenSession} upstreamName={upstreamName} sessionParent={parent ? { label: parent.label, sessionId: parent.identity.session_id, keyId: parent.identity.key_id } : undefined} />
    <RequestEgressDetails attempts={detail.egress_attempts} accountNames={accountNames} />
    <div className="request-diagnostics request-detail-surface request-archive-diagnostics">
      <span><b>{t('self.archive')}</b>{detail.archive_complete ? t('request.archiveComplete') : t('request.archiveIncomplete')}</span>
      {detail.provenance && <span><b>{t('request.provenance')}</b>{detail.provenance.unlinked ? t('request.archiveOnly') : t('request.exactArchive')} · {detail.provenance.source}</span>}
    </div>
    <Button appearance="subtle" aria-expanded={technicalOpen} aria-controls={technicalId} onClick={() => setTechnicalOpen(!technicalOpen)}>{t('request.technicalDetails')}</Button>
    {technicalOpen && <section id={technicalId} className="request-technical-details" aria-label={t('request.technicalDetails')}>
      <h3>{t('request.rawRequestJson')}</h3><pre>{JSON.stringify(detail.request_body, null, 2)}</pre>
      <h3>{t('request.rawReturnJson')}</h3><pre>{JSON.stringify(detail.response_body, null, 2)}</pre>
      {detail.provenance && <><h3>{t('request.provenance')}</h3><pre>{JSON.stringify(detail.provenance, null, 2)}</pre></>}
      {detail.transport_diagnostics?.length ? <><h3>{locale === 'zh-CN' ? '上游传输诊断' : 'Upstream transport diagnostics'}</h3><pre>{JSON.stringify(detail.transport_diagnostics, null, 2)}</pre></> : null}
    </section>}
  </DrawerFrame>;
}
