import { useEffect, useId, useMemo, useRef, useState } from 'react';
import { Button, ToggleButton } from '@fluentui/react-components';
import { DetailTooltip } from '../../design-system';
import { api, apiDiagnosticMessage } from '../../api';
import { DrawerFrame, RequestDiagnostics, RequestTable } from '../../components';
import { formatCurrencyDisplay, formatDurationDisplay, formatMetricDisplay, formatPercent } from '../../format';
import { LocalSettlementNotice, localSettlementLabel } from '../../LocalSettlementNotice';
import { AnalyticsMetric } from '../AnalyticsMetric';
import { ImageGenerationQuarantine } from '../ImageGenerationQuarantine';
import { displayTimeZone } from '../../charts/displayTimeZone';
import { useI18n } from '../../i18n';
import type { RequestDetail, RequestEvent, RequestListResponse, RequestView, TypedFilterAst, UpstreamAccount } from '../../types';
import type { SessionStreamState } from '../SessionMonitor';
import { queryForTenant } from '../scope/operatorShared';
import { TypedFilterBuilder } from '../TypedFilterBuilder';
import { RequestRefreshControl } from '../traffic/RequestRefreshControl';
import { mergeBatchedRequestPage } from '../traffic/requestRefresh';
import {
  emptyTypedFilterAst, filteredRequestRefreshDelay, mergeRefreshedRequestPage,
  summarizeVisibleRequests, typedFiltersActive, typedRequestQueryBody, visibleRequestMetricSeries,
} from '../traffic/requestTraffic';

export function RequestsPage({ token, tenant, writeTenant = tenant, liveEvents, streamRevision, streamState, streamError, onOpenSessions, onOpenSession, requestFocus, onRequestFocusHandled, requestDrilldown, onRequestDrilldownHandled, requestRefresh, streamOverflowRevision = 0, onProtectRequests }: {
  token: string;
  tenant: string;
  writeTenant?: string;
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
  requestRefresh?: { intervalMs: number; paused: boolean; onIntervalChange: (value: number) => void };
  streamOverflowRevision?: number;
  onProtectRequests?: (ids: string[], consumerPaused?: boolean) => void;
}) {
  const { t } = useI18n();
  const diagnosticLabels = { requestId: t('request.correlationId'), streamInterrupted: t('request.streamInterrupted') };
  const [requests, setRequests] = useState<RequestView[]>([]);
  const [upstreams, setUpstreams] = useState<UpstreamAccount[]>([]);
  const [filters, setFilters] = useState<TypedFilterAst>(emptyTypedFilterAst);
  const [loading, setLoading] = useState(false);
  const [hasOlder, setHasOlder] = useState(false);
  const [detailResult, setDetail] = useState<{ value: RequestDetail; token: string; tenant: string; requestId: string }>();
  const [error, setError] = useState('');
  const [upstreamError, setUpstreamError] = useState('');
  const [olderFilteredResultsStale, setOlderFilteredResultsStale] = useState(false);
  const [imageReviewOpen, setImageReviewOpen] = useState(false);
  const sequence = useRef(0);
  const errorSource = useRef<'detail' | 'load' | 'refresh' | undefined>(undefined);
  const olderFilteredResultsVisible = useRef(false);
  const loadedHistoryIds = useRef(new Set<string>());
  const requestsRef = useRef(requests);
  const loadingRef = useRef(false);
  const paused = useRef(false);
  // The pause tier is page-local: SSE and the bounded hook buffer keep running
  // while the visible list, metrics and drawer stay frozen until resume.
  const [userPaused, setUserPaused] = useState(false);
  const streamPaused = userPaused || (requestRefresh?.paused ?? false);
  paused.current = streamPaused;
  const reconcileOverflow = useRef(false);
  const previousOverflowRevision = useRef(streamOverflowRevision);
  const overflowRefresh = useRef<{
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
  const selectedRequestId = useRef<string | undefined>(undefined);
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

  function cancelOverflowRefresh() {
    const state = overflowRefresh.current;
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

  // Overflow reconciliation serves only the unfiltered live first page.
  // Filtered and history views stay stable until an explicit manual refresh.
  function scheduleOverflowRefresh() {
    const state = overflowRefresh.current;
    const currentScope = scope.current;
    if (!currentScope.token || !currentScope.tenant || typedFiltersActive(currentScope.filters) || !reconcileOverflow.current) return;
    state.pending = true;
    if (state.inFlight || loadingRef.current || paused.current) return;

    const now = Date.now();
    state.firstPendingAt ??= now;
    if (state.timer !== undefined) window.clearTimeout(state.timer);
    const scopeSequence = state.scopeSequence;
    state.timer = window.setTimeout(() => {
      state.timer = undefined;
      if (overflowRefresh.current.scopeSequence === scopeSequence) void refreshOverflowFirstPage();
    }, filteredRequestRefreshDelay(now, state.firstPendingAt));
  }

  async function refreshOverflowFirstPage() {
    const state = overflowRefresh.current;
    if (state.inFlight || loadingRef.current || !state.pending || paused.current) return;
    const currentScope = scope.current;
    if (!currentScope.token || !currentScope.tenant || typedFiltersActive(currentScope.filters) || !reconcileOverflow.current) return;
    state.inFlight = true;
    state.pending = false;
    state.firstPendingAt = undefined;
    const requestSequence = ++state.requestSequence;
    const scopeSequence = state.scopeSequence;
    const overflowAtStart = previousOverflowRevision.current;
    const controller = new AbortController();
    state.controller = controller;
    try {
      const next = await api<RequestListResponse>('/internal/v1/requests/query', currentScope.token, {
        method: 'POST', body: JSON.stringify(typedRequestQueryBody(currentScope.tenant, currentScope.filters)), signal: controller.signal,
      });
      const latest = scope.current;
      if (controller.signal.aborted || state.requestSequence !== requestSequence || state.scopeSequence !== scopeSequence
        || latest.token !== currentScope.token || latest.tenant !== currentScope.tenant || latest.filters !== currentScope.filters) return;
      // Replace the live window with the authoritative server first page, then
      // re-apply the bounded live buffer. Never runs for filtered views.
      const current = requestsRef.current;
      const page = mergeBatchedRequestPage(mergeRefreshedRequestPage(current, next, current.length > 100), new Map(liveEventsRef.current), next.next_cursor !== null, loadedHistoryIds.current);
      setRequests(page.requests); setHasOlder(page.hasOlder);
      if (previousOverflowRevision.current === overflowAtStart) reconcileOverflow.current = false;
      if (errorSource.current === 'refresh') {
        errorSource.current = undefined;
        setError('');
      }
    } catch (reason) {
      if (!controller.signal.aborted && state.requestSequence === requestSequence && state.scopeSequence === scopeSequence) {
        errorSource.current = 'refresh';
        setError(apiDiagnosticMessage(reason, t('common.requestFailed'), diagnosticLabels));
      }
    } finally {
      if (state.requestSequence !== requestSequence || state.scopeSequence !== scopeSequence) return;
      state.controller = undefined;
      state.inFlight = false;
      if (state.pending) scheduleOverflowRefresh();
    }
  }

  async function load(nextFilters: TypedFilterAst, older = false) {
    if (!token || !tenant) return;
    const last = requests.at(-1);
    const before = last ? { before_created_at: last.created_at, before_id: last.request_id } : undefined;
    if (older && (!hasOlder || !before)) return;
    // Freeze insertion before the page fetch, not after it resolves: otherwise
    // a live batch could move the visible tail while this cursor is in flight.
    if (older) for (const request of requestsRef.current) loadedHistoryIds.current.add(request.request_id);
    const refreshWasActive = older && (overflowRefresh.current.pending || overflowRefresh.current.inFlight);
    // A foreground page request owns the request list until it settles. Abort
    // any background first-page refresh rather than running two query POSTs.
    cancelOverflowRefresh();
    loadAbort.current?.abort();
    const controller = new AbortController();
    loadAbort.current = controller;
    const request = ++sequence.current;
    const currentScope = { token, tenant, filters: nextFilters };
    if (!older) {
      loadedHistoryIds.current.clear();
      olderFilteredResultsVisible.current = false;
      setOlderFilteredResultsStale(false);
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
      if (older && typedFiltersActive(nextFilters) && next.requests.length > 0) olderFilteredResultsVisible.current = true;
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
        if (refreshWasActive) overflowRefresh.current.pending = true;
        if (overflowRefresh.current.pending) scheduleOverflowRefresh();
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
    reconcileOverflow.current = false;
    previousOverflowRevision.current = streamOverflowRevision;
    onProtectRequests?.([]);
    cancelOverflowRefresh();
    loadAbort.current?.abort(); loadAbort.current = null;
    olderFilteredResultsVisible.current = false;
    errorSource.current = undefined;
    loadingRef.current = false;
    sequence.current += 1; setFilters(emptyTypedFilterAst); setRequests([]); setDetail(undefined); setHasOlder(false); setLoading(false); setOlderFilteredResultsStale(false); setImageReviewOpen(false); setError(''); setUpstreamError('');
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
    detailSequence.current += 1; detailAbort.current?.abort(); detailAbort.current = null; setDetail(undefined);
    return () => { detailSequence.current += 1; detailAbort.current?.abort(); };
  }, [tenant, token]);

  useEffect(() => {
    // Report the pause tier together with the visible ids: while paused the
    // hook batch stops publishing revisions entirely but keeps buffering SSE.
    onProtectRequests?.(requests.map(request => request.request_id), userPaused);
  }, [requests, userPaused, onProtectRequests]);
  useEffect(() => () => onProtectRequests?.([]), [onProtectRequests]);

  useEffect(() => {
    if (streamPaused) {
      const state = overflowRefresh.current;
      const pending = state.pending || state.inFlight;
      cancelOverflowRefresh();
      state.pending = pending;
    } else if (overflowRefresh.current.pending) scheduleOverflowRefresh();
  }, [streamPaused]);

  useEffect(() => {
    if (streamOverflowRevision !== previousOverflowRevision.current) {
      previousOverflowRevision.current = streamOverflowRevision;
      // Overflow reconciliation serves only the unfiltered live first page.
      // Filtered and history views stay stable until an explicit refresh.
      if (typedFiltersActive(filters) || loadedHistoryIds.current.size) return;
      reconcileOverflow.current = true;
      scheduleOverflowRefresh();
    }
  }, [streamOverflowRevision]);

  useEffect(() => {
    // SSE batches never auto-refresh a filtered view. They only surface the
    // stale hint so the user can explicitly reload filtered history.
    if (streamPaused || liveEvents.size === 0) return;
    if (typedFiltersActive(filters)) {
      const terminalizedVisiblePending = olderFilteredResultsVisible.current
        && [...liveEventsRef.current.values()].some((event) => (event.event_kind === 'finished' || event.completed_at != null)
          && requestsRef.current.some((request) => request.request_id === event.request_id && request.status_code === null));
      if (terminalizedVisiblePending) setOlderFilteredResultsStale(true);
      return;
    }
    const page = mergeBatchedRequestPage(requestsRef.current, new Map(liveEventsRef.current), hasOlderRef.current, loadedHistoryIds.current);
    setRequests(page.requests); setHasOlder(page.hasOlder);
  }, [streamRevision, streamPaused]);

  useEffect(() => {
    if (!loading && overflowRefresh.current.pending) scheduleOverflowRefresh();
  }, [loading]);

  async function openRequestDetail(requestId: string) {
    if (selectedRequestId.current !== requestId) setDetail(undefined);
    selectedRequestId.current = requestId;
    const requestSequence = ++detailSequence.current;
    detailAbort.current?.abort(); const controller = new AbortController(); detailAbort.current = controller;
    try {
      errorSource.current = undefined;
      setError('');
      const next = await api<RequestDetail>(`/internal/v1/requests/${requestId}${queryForTenant(tenant)}`, token, { signal: controller.signal });
      if (requestSequence === detailSequence.current && selectedRequestId.current === requestId && !controller.signal.aborted) setDetail({ value: next, token, tenant, requestId });
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
    void openRequestDetail(requestFocus.requestId);
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

  return <>
    {error && <div className="notice error" role="alert">{t(errorSource.current === 'detail' ? 'request.detail' : 'request.listSource')}: {error}</div>}
    {upstreamError && <div className="notice error" role="alert">{t('request.upstreamSource')}: {upstreamError}</div>}
    {streamError && <div className="notice error" role="alert">{t('request.streamSource')}: {streamError}</div>}
    <RequestsPanel requests={requests} upstreams={upstreams} filters={filters} loading={loading} hasOlder={hasOlder} streamState={streamState} token={token} tenant={tenant} olderFilteredResultsStale={olderFilteredResultsStale} requestRefresh={requestRefresh} refreshPaused={userPaused} onToggleRefreshPaused={() => setUserPaused((value) => !value)} historyLoaded={loadedHistoryIds.current.size > 0} imageReviewOpen={imageReviewOpen} onToggleImageReview={() => setImageReviewOpen((open) => !open)}
      onApply={(next) => { setFilters(next); scope.current = { token, tenant, filters: next }; void load(next); }}
      onClear={() => { setFilters(emptyTypedFilterAst); scope.current = { token, tenant, filters: emptyTypedFilterAst }; void load(emptyTypedFilterAst); }}
      onLoadOlder={() => void load(filters, true)} onRefreshFilteredResults={() => void load(filters)} onSelect={selectRequest} onOpenSessions={onOpenSessions} onOpenSession={onOpenSession} />
    {imageReviewOpen && <section className="request-image-review" aria-label={t('quarantine.title')}>
      <ImageGenerationQuarantine token={token} tenant={tenant} writeTenant={writeTenant} />
    </section>}
    {detail && <RequestDrawer detail={detail} upstreamName={upstreams.find((account) => account.id === detail.upstream_account_id)?.name} onOpenSession={onOpenSession} onClose={closeRequestDetail} />}
  </>;
}

function RequestsPanel({ requests, upstreams, filters, loading, hasOlder, streamState, token, tenant, olderFilteredResultsStale, onApply, onClear, onLoadOlder, onRefreshFilteredResults, onSelect, onOpenSessions, onOpenSession, requestRefresh, refreshPaused = false, onToggleRefreshPaused, historyLoaded, imageReviewOpen, onToggleImageReview }: {
  requests: RequestView[];
  upstreams: UpstreamAccount[];
  filters: TypedFilterAst;
  loading: boolean;
  hasOlder: boolean;
  streamState: SessionStreamState;
  token: string;
  tenant: string;
  olderFilteredResultsStale: boolean;
  historyLoaded: boolean;
  imageReviewOpen: boolean;
  onToggleImageReview: () => void;
  onApply: (filters: TypedFilterAst) => void;
  onClear: () => void;
  onLoadOlder: () => void;
  onRefreshFilteredResults: () => void;
  onSelect: (request: RequestView) => Promise<void>;
  onOpenSessions: () => void;
  onOpenSession: (sessionId: string) => void;
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
  return <article className="panel request-page-surface"><div className="panel-title traffic-heading"><div><h2>{typedFiltersActive(filters) ? t('traffic.filtered') : t('traffic.live')}</h2><span>{typedFiltersActive(filters) ? t('traffic.filteredHint') : t('traffic.liveHint')}</span></div><div className="traffic-heading-actions"><Button appearance="secondary" aria-expanded={imageReviewOpen} onClick={onToggleImageReview}>{t('quarantine.menuItem')}</Button><div className={`request-live-state session-live-state ${streamState}`} role="status">{t(`sessions.live.${streamState}`)}</div><div className="segmented" role="group" aria-label={t('sessions.monitorMode')}><ToggleButton appearance="subtle" checked>{t('sessions.requestsMode')}</ToggleButton><ToggleButton appearance="subtle" checked={false} onClick={onOpenSessions}>{t('sessions.sessionsMode')}</ToggleButton></div></div></div>
    {requestRefresh && <div className="request-refresh-row">
      <RequestRefreshControl intervalMs={requestRefresh.intervalMs} onIntervalChange={requestRefresh.onIntervalChange}
        paused={requestRefresh.paused || refreshPaused}
        pausedHint={refreshPaused && !requestRefresh.paused
          ? (locale === 'zh-CN' ? '已暂停更新：事件接收继续，恢复后合并。' : 'Updates paused: events keep streaming and merge on resume.')
          : undefined} />
      {onToggleRefreshPaused && <span className="request-refresh-pause">
        <ToggleButton appearance="secondary" checked={refreshPaused} onClick={onToggleRefreshPaused}>{refreshPaused ? (locale === 'zh-CN' ? '恢复更新' : 'Resume updates') : (locale === 'zh-CN' ? '暂停更新' : 'Pause updates')}</ToggleButton>
      </span>}
    </div>}
    {historyLoaded && !typedFiltersActive(filters) && <div className="request-refresh-control"><span>{locale === 'zh-CN' ? '正在浏览历史：已显示请求继续更新，新请求暂不插入。' : 'Browsing history: visible requests keep updating; new requests are not inserted.'}</span><Button appearance="subtle" disabled={loading} onClick={onRefreshFilteredResults}>{locale === 'zh-CN' ? '返回最新请求' : 'Return to latest requests'}</Button></div>}
    <TypedFilterBuilder ast={filters} disabled={loading} onApply={onApply} onClear={onClear} scope="requests" token={token} tenant={tenant} upstreams={upstreams} />
    {olderFilteredResultsStale && <div className="notice warning" role="status">{t('traffic.olderFilteredResultsStale')}<Button appearance="secondary" disabled={loading} onClick={onRefreshFilteredResults}>{t('traffic.refreshFilteredResults')}</Button></div>}
    {requests.length > 0 && <section className="metrics request-traffic-metrics" aria-label={t('monitoring.summary')}>
      <AnalyticsMetric {...sampling} label={t('usage.totalTokens')} value={count(summary.totalTokens).text} title={count(summary.totalTokens).title} trend={points.map(point => point.totalTokens)} />
      <AnalyticsMetric {...sampling} label={t('traffic.success')} value={count(summary.successful).text} title={count(summary.successful).title} tone="positive" trend={points.map(point => point.successful)} ratio={summary.successful / summary.requests} />
      <AnalyticsMetric {...sampling} label={localSettlementLabel(locale)} labelContent={<LocalSettlementNotice />} value={summary.localCosts.length ? <span className="usage-cost-lines">{summary.localCosts.map(({ currency, cost }) => { const display = formatCurrencyDisplay(cost, currency, locale); return <span key={currency} title={display.title}>{display.text}</span>; })}</span> : '—'} formatSample={(_value, index) => { const cost = settlementCurrency ? points[index].localCosts.find(item => item.currency === settlementCurrency) : undefined; return cost ? formatCurrencyDisplay(cost.cost, cost.currency, locale).title ?? '—' : '—'; }} trend={settlementCurrency ? points.map((point) => { const cost = point.localCosts.find(item => item.currency === settlementCurrency); return cost ? cost.cost : null; }) : undefined} />
      <AnalyticsMetric {...sampling} label={t('common.running')} value={count(summary.running).text} title={count(summary.running).title} trend={points.map(point => point.running)} ratio={summary.running / summary.requests} />
      {summary.unknown > 0 && <AnalyticsMetric {...sampling} label={locale === 'zh-CN' ? '终态未知' : 'Outcome unknown'} value={count(summary.unknown).text} title={count(summary.unknown).title} trend={points.map(point => point.unknown)} ratio={summary.unknown / summary.requests} />}
      <AnalyticsMetric label={locale === 'zh-CN' ? '已结束请求成功率' : 'Finished request success rate'} labelContent={<DetailTooltip content={locale === 'zh-CN' ? '仅当前已加载记录：成功 ÷（成功 + 非成功）。客户端断开、取消、中断和失败计入非成功；运行中、交付中和终态未知不进入分母。无明确终态时显示 —。' : 'Loaded records only: successful ÷ (successful + unsuccessful). Client disconnection, cancellation, interruption and failure count as unsuccessful. Running, delivering and unknown outcomes are excluded. Shows — when no terminal outcome is known.'}><span tabIndex={0}>{locale === 'zh-CN' ? '已结束请求成功率' : 'Finished request success rate'}</span></DetailTooltip>} value={formatPercent(summary.successRate, locale)} ratio={summary.successRate} />
      <AnalyticsMetric {...sampling} label={t('usage.average')} value={averageDuration.text} title={averageDuration.title} trend={points.map(point => point.averageDurationMs)} formatSample={value => { const display = formatDurationDisplay(value, locale); return display.title ?? display.text; }} />
    </section>}
    {requests.length > 0 && <p className="request-metrics-scope">{locale === 'zh-CN' ? '仅统计当前已加载请求；背景图按接收时间展示这些记录的分布，不代表全量流量。' : 'Loaded requests only. Background charts group these records by reception time, not total traffic.'}</p>}
    {loading && requests.length === 0 ? <div className="empty" role="status">{t('common.loading')}</div> : <RequestTable requests={requests} upstreamNames={new Map(upstreams.map((account) => [account.id, account.name]))} onSelect={(request) => void onSelect(request)} onOpenSession={onOpenSession} />}
    {hasOlder && <div className="load-more"><Button appearance="secondary" disabled={loading} onClick={onLoadOlder}>{loading ? t('common.loading') : t('traffic.loadOlder')}</Button></div>}
  </article>;
}

function RequestDrawer({ detail, upstreamName, onOpenSession, onClose }: { detail: RequestDetail; upstreamName?: string; onOpenSession: (sessionId: string) => void; onClose: () => void }) {
  const { t } = useI18n();
  const [technicalOpen, setTechnicalOpen] = useState(false);
  const technicalId = useId();
  return <DrawerFrame title={detail.model} eyebrow={t('request.operatorDiagnosis')} onClose={onClose}>
    <RequestDiagnostics request={detail} onOpenSession={onOpenSession} upstreamName={upstreamName} />
    <div className="request-diagnostics request-detail-surface request-archive-diagnostics">
      <span><b>{t('self.archive')}</b>{detail.archive_complete ? t('request.archiveComplete') : t('request.archiveIncomplete')}</span>
      {detail.provenance && <span><b>{t('request.provenance')}</b>{detail.provenance.unlinked ? t('request.archiveOnly') : t('request.exactArchive')} · {detail.provenance.source}</span>}
    </div>
    <Button appearance="subtle" aria-expanded={technicalOpen} aria-controls={technicalId} onClick={() => setTechnicalOpen(!technicalOpen)}>{t('request.technicalDetails')}</Button>
    {technicalOpen && <section id={technicalId} className="request-technical-details" aria-label={t('request.technicalDetails')}>
      <h3>{t('request.request')}</h3><pre>{JSON.stringify(detail.request_body, null, 2)}</pre>
      <h3>{t('request.response')}</h3><pre>{JSON.stringify(detail.response_body, null, 2)}</pre>
      {detail.provenance && <><h3>{t('request.provenance')}</h3><pre>{JSON.stringify(detail.provenance, null, 2)}</pre></>}
    </section>}
  </DrawerFrame>;
}
