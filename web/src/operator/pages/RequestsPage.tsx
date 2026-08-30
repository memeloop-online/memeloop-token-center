import { useEffect, useRef, useState } from 'react';
import { api } from '../../api';
import { DrawerFrame, RequestTable } from '../../components';
import { useI18n } from '../../i18n';
import type { RequestDetail, RequestEvent, RequestView, UpstreamAccount } from '../../types';
import type { SessionStreamState } from '../SessionMonitor';
import { messageOf, queryForTenant } from '../scope/operatorShared';
import {
  emptyRequestFilters,
  filtersActive,
  mergeLiveRequestEvents,
  requestQuery,
  type RequestFilters,
} from '../traffic/requestTraffic';

export function RequestsPage({ token, tenant, liveEvents, streamRevision, streamState, streamError, onOpenSessions, onOpenSession }: {
  token: string;
  tenant: string;
  liveEvents: ReadonlyMap<string, RequestEvent>;
  streamRevision: number;
  streamState: SessionStreamState;
  streamError: string;
  onOpenSessions: () => void;
  onOpenSession: (sessionId: string) => void;
}) {
  const { t } = useI18n();
  const [requests, setRequests] = useState<RequestView[]>([]);
  const [upstreams, setUpstreams] = useState<UpstreamAccount[]>([]);
  const [filters, setFilters] = useState<RequestFilters>(emptyRequestFilters);
  const [loading, setLoading] = useState(false);
  const [hasOlder, setHasOlder] = useState(false);
  const [detail, setDetail] = useState<RequestDetail>();
  const [error, setError] = useState('');
  const [upstreamError, setUpstreamError] = useState('');
  const sequence = useRef(0);
  const upstreamSequence = useRef(0);
  const detailSequence = useRef(0);
  const detailAbort = useRef<AbortController | null>(null);
  const scope = useRef({ token, tenant, filters });
  scope.current = { token, tenant, filters };

  async function load(nextFilters: RequestFilters, older = false) {
    if (!token) return;
    const request = ++sequence.current;
    const currentScope = { token, tenant, filters: nextFilters };
    const before = older ? requests.at(-1) : undefined;
    if (!older) { setRequests([]); setHasOlder(false); setDetail(undefined); }
    setLoading(true);
    setError('');
    try {
      const next = await api<RequestView[]>(
        `/internal/v1/requests${requestQuery(tenant, nextFilters, before)}`,
        token,
      );
      const latest = scope.current;
      if (request !== sequence.current || latest.token !== currentScope.token
        || latest.tenant !== currentScope.tenant || latest.filters !== currentScope.filters) return;
      setRequests((current) => older
        ? [...current, ...next.filter((value) => !current.some((existing) => existing.request_id === value.request_id))]
        : filtersActive(nextFilters) ? next : mergeLiveRequestEvents(next, new Map(liveEvents)));
      setHasOlder(next.length === 100);
    } catch (reason) {
      if (request === sequence.current) {
        if (!older) { setRequests([]); setHasOlder(false); }
        setError(messageOf(reason, t('common.requestFailed')));
      }
    } finally {
      if (request === sequence.current) setLoading(false);
    }
  }

  useEffect(() => {
    sequence.current += 1;
    setFilters(emptyRequestFilters);
    setRequests([]);
    setDetail(undefined);
    setHasOlder(false);
    setError('');
    setUpstreamError('');
    if (!token) {
      setUpstreams([]);
      return;
    }
    const upstreamRequest = ++upstreamSequence.current;
    void api<UpstreamAccount[]>(`/internal/v1/upstreams${queryForTenant(tenant)}`, token)
      .then((values) => { if (upstreamRequest === upstreamSequence.current) { setUpstreams(values); setUpstreamError(''); } })
      .catch((reason) => { if (upstreamRequest === upstreamSequence.current) { setUpstreams([]); setUpstreamError(messageOf(reason, t('common.requestFailed'))); } });
    void load(emptyRequestFilters);
  }, [tenant, token]);

  useEffect(() => {
    detailSequence.current += 1;
    detailAbort.current?.abort();
    detailAbort.current = null;
    setDetail(undefined);
    return () => {
      detailSequence.current += 1;
      detailAbort.current?.abort();
    };
  }, [tenant, token]);

  useEffect(() => {
    if (liveEvents.size === 0 || filtersActive(filters)) return;
    // React may batch several replay revisions into one render. Merge the
    // complete bounded event map so no intermediate SSE event disappears.
    setRequests((current) => mergeLiveRequestEvents(current, new Map(liveEvents)));
  }, [streamRevision]);

  async function selectRequest(request: RequestView) {
    const requestSequence = ++detailSequence.current;
    detailAbort.current?.abort();
    const controller = new AbortController();
    detailAbort.current = controller;
    try {
      setError('');
      const next = await api<RequestDetail>(
        `/internal/v1/requests/${request.request_id}${queryForTenant(tenant)}`,
        token,
        { signal: controller.signal },
      );
      if (requestSequence === detailSequence.current) setDetail(next);
    } catch (reason) {
      if (requestSequence === detailSequence.current && !controller.signal.aborted) {
        setError(messageOf(reason, t('traffic.detailFailed')));
      }
    } finally {
      if (detailAbort.current === controller) detailAbort.current = null;
    }
  }

  return <>
    {error && <div className="notice error" role="alert">{error}</div>}
    {upstreamError && <div className="notice error" role="alert">{upstreamError}</div>}
    {streamError && <div className="notice error" role="alert">{streamError}</div>}
    <RequestsPanel
      requests={requests}
      upstreams={upstreams}
      upstreamsAvailable={!upstreamError}
      filters={filters}
      loading={loading}
      hasOlder={hasOlder}
      streamState={streamState}
      onApply={(next) => { setFilters(next); scope.current = { token, tenant, filters: next }; void load(next); }}
      onClear={() => { setFilters(emptyRequestFilters); scope.current = { token, tenant, filters: emptyRequestFilters }; void load(emptyRequestFilters); }}
      onLoadOlder={() => void load(filters, true)}
      onSelect={selectRequest}
      onOpenSessions={onOpenSessions}
      onOpenSession={onOpenSession}
    />
    {detail && <RequestDrawer detail={detail} onClose={() => setDetail(undefined)} />}
  </>;
}

function RequestsPanel({ requests, upstreams, upstreamsAvailable, filters, loading, hasOlder, streamState, onApply, onClear, onLoadOlder, onSelect, onOpenSessions, onOpenSession }: {
  requests: RequestView[];
  upstreams: UpstreamAccount[];
  upstreamsAvailable: boolean;
  filters: RequestFilters;
  loading: boolean;
  hasOlder: boolean;
  streamState: SessionStreamState;
  onApply: (filters: RequestFilters) => void;
  onClear: () => void;
  onLoadOlder: () => void;
  onSelect: (request: RequestView) => Promise<void>;
  onOpenSessions: () => void;
  onOpenSession: (sessionId: string) => void;
}) {
  const { t } = useI18n();
  const [draft, setDraft] = useState(filters);
  useEffect(() => setDraft(filters), [filters]);

  return <article className="panel"><div className="panel-title traffic-heading"><div><h2>{filtersActive(filters) ? t('traffic.filtered') : t('traffic.live')}</h2><span>{filtersActive(filters) ? t('traffic.filteredHint') : t('traffic.liveHint')}</span></div><div className="traffic-heading-actions"><div className={`request-live-state session-live-state ${streamState}`} role="status">{t(`sessions.live.${streamState}`)}</div><div className="segmented" role="group" aria-label={t('sessions.monitorMode')}><button type="button" className="active" aria-pressed="true">{t('sessions.requestsMode')}</button><button type="button" aria-pressed="false" onClick={onOpenSessions}>{t('sessions.sessionsMode')}</button></div></div></div>
    <form className="traffic-filters" onSubmit={(event) => { event.preventDefault(); onApply(draft); }}>
      <label>{t('traffic.from')}<input type="datetime-local" value={draft.from} onChange={(event) => setDraft({ ...draft, from: event.target.value })} /></label>
      <label>{t('traffic.to')}<input type="datetime-local" value={draft.to} onChange={(event) => setDraft({ ...draft, to: event.target.value })} /></label>
      <label>{t('traffic.keyId')}<input value={draft.keyId} onChange={(event) => setDraft({ ...draft, keyId: event.target.value })} placeholder="019f…" /></label>
      <label>{t('request.model')}<input value={draft.model} onChange={(event) => setDraft({ ...draft, model: event.target.value })} /></label>
      <label>{t('request.protocol')}<select value={draft.protocol} onChange={(event) => setDraft({ ...draft, protocol: event.target.value })}><option value="">{t('common.all')}</option><option value="openai">OpenAI</option><option value="anthropic">Anthropic</option><option value="openai-image">OpenAI Image</option><option value="generation">{t('routes.generation')}</option></select></label>
      <label>{t('request.status')}<select value={draft.status} onChange={(event) => setDraft({ ...draft, status: event.target.value })}><option value="">{t('common.all')}</option><option value="success">{t('traffic.success')}</option><option value="error">{t('traffic.failure')}</option><option value="pending">{t('common.running')}</option></select></label>
      <label>{t('traffic.errorCode')}<input value={draft.errorCode} onChange={(event) => setDraft({ ...draft, errorCode: event.target.value })} /></label>
      <label>{t('traffic.upstream')}<select disabled={!upstreamsAvailable} value={draft.upstreamAccountId} onChange={(event) => setDraft({ ...draft, upstreamAccountId: event.target.value })}><option value="">{t('common.all')}</option>{upstreams.map((value) => <option value={value.id} key={value.id}>{value.name}</option>)}</select></label>
      <label>{t('traffic.routeId')}<input value={draft.routeId} onChange={(event) => setDraft({ ...draft, routeId: event.target.value })} placeholder="019f…" /></label>
      <label>{t('traffic.keyAlias')}<input value={draft.keyAlias} onChange={(event) => setDraft({ ...draft, keyAlias: event.target.value })} /></label>
      <label>{t('traffic.principal')}<input value={draft.principal} onChange={(event) => setDraft({ ...draft, principal: event.target.value })} /></label>
      <label>{t('traffic.minDuration')}<input type="number" min="0" value={draft.minDurationMs} onChange={(event) => setDraft({ ...draft, minDurationMs: event.target.value })} /></label>
      <label>{t('traffic.maxDuration')}<input type="number" min="0" value={draft.maxDurationMs} onChange={(event) => setDraft({ ...draft, maxDurationMs: event.target.value })} /></label>
      <label>{t('traffic.minCost')}<input inputMode="decimal" value={draft.minCost} onChange={(event) => setDraft({ ...draft, minCost: event.target.value })} /></label>
      <label>{t('traffic.maxCost')}<input inputMode="decimal" value={draft.maxCost} onChange={(event) => setDraft({ ...draft, maxCost: event.target.value })} /></label>
      <div className="filter-actions"><button type="submit" disabled={loading}>{loading ? t('common.loading') : t('traffic.applyFilters')}</button><button type="button" className="secondary" disabled={loading || (!filtersActive(filters) && !filtersActive(draft))} onClick={() => { setDraft(emptyRequestFilters); onClear(); }}>{t('traffic.clearFilters')}</button></div>
    </form>
    <RequestTable requests={requests} onSelect={(request) => void onSelect(request)} onOpenSession={onOpenSession} />
    {hasOlder && <div className="load-more"><button type="button" className="secondary" disabled={loading} onClick={onLoadOlder}>{loading ? t('common.loading') : t('traffic.loadOlder')}</button></div>}
  </article>;
}

function RequestDrawer({ detail, onClose }: { detail: RequestDetail; onClose: () => void }) {
  const { t } = useI18n();
  return <DrawerFrame title={detail.model} eyebrow={t('request.operatorDiagnosis')} onClose={onClose}><p className="muted break-anywhere">{detail.request_id} · {detail.status_code ?? t('common.running')} · {detail.archive_complete ? t('request.archiveComplete') : t('request.archiveIncomplete')}</p><h3>{t('request.error')}</h3><pre>{detail.error_code ?? t('common.none')}</pre><h3>{t('request.request')}</h3><pre>{JSON.stringify(detail.request_body, null, 2)}</pre><h3>{t('request.response')}</h3><pre>{JSON.stringify(detail.response_body, null, 2)}</pre></DrawerFrame>;
}
