import { useEffect, useState, type FormEvent } from 'react';
import { ApiError, api } from '../api';
import { Buckets, DrawerFrame, Metric, RequestTable, Shell } from '../components';
import { useI18n } from '../i18n';
import type { ConversationCluster, ConversationDetail, GenerationJob, KeyView, RequestDetail, RequestView, SelfStats } from '../types';

const requestPageSize = 50;

interface RequestFilters {
  from: string;
  to: string;
  model: string;
  protocol: string;
  status: string;
  errorCode: string;
  upstreamAccountId: string;
  routeId: string;
  minDurationMs: string;
  maxDurationMs: string;
  minCost: string;
  maxCost: string;
}

const emptyRequestFilters: RequestFilters = {
  from: '',
  to: '',
  model: '',
  protocol: '',
  status: '',
  errorCode: '',
  upstreamAccountId: '',
  routeId: '',
  minDurationMs: '',
  maxDurationMs: '',
  minCost: '',
  maxCost: '',
};

function requestsPath(filters: RequestFilters, before?: RequestView) {
  const query = new URLSearchParams({ limit: String(requestPageSize) });
  const from = filters.from ? new Date(filters.from).getTime() : Number.NaN;
  const parsedTo = filters.to ? new Date(filters.to).getTime() : Number.NaN;
  const to = Number.isFinite(parsedTo) && /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}$/.test(filters.to)
    ? parsedTo + 59_999
    : parsedTo;
  if (Number.isFinite(from)) query.set('from_created_at', String(from));
  if (Number.isFinite(to)) query.set('to_created_at', String(to));
  if (filters.model.trim()) query.set('model', filters.model.trim());
  if (filters.protocol.trim()) query.set('protocol', filters.protocol.trim());
  if (filters.status) query.set('status', filters.status);
  if (filters.errorCode.trim()) query.set('error_code', filters.errorCode.trim());
  if (filters.upstreamAccountId.trim()) query.set('upstream_account_id', filters.upstreamAccountId.trim());
  if (filters.routeId.trim()) query.set('route_id', filters.routeId.trim());
  if (filters.minDurationMs.trim()) query.set('min_duration_ms', filters.minDurationMs.trim());
  if (filters.maxDurationMs.trim()) query.set('max_duration_ms', filters.maxDurationMs.trim());
  if (filters.minCost.trim()) query.set('min_cost', filters.minCost.trim());
  if (filters.maxCost.trim()) query.set('max_cost', filters.maxCost.trim());
  if (before) {
    query.set('before_created_at', String(before.created_at));
    query.set('before_id', before.request_id);
  }
  return `/self/v1/requests?${query}`;
}

function statsPath(filters: RequestFilters) {
  const query = new URLSearchParams(requestsPath(filters).split('?')[1]);
  query.delete('limit');
  query.delete('before_created_at');
  query.delete('before_id');
  return `/self/v1/stats?${query}`;
}

function selfErrorMessage(reason: unknown, t: (key: string) => string, fallback: string) {
  if (reason instanceof ApiError) {
    if (reason.code === 'unauthorized' || reason.status === 401) return t('self.invalidCredential');
    if (reason.code === 'invalid_request' || reason.status === 400) return t('self.invalidFilter');
    if (reason.code === 'forbidden' || reason.status === 403) return t('self.readPermissionDenied');
    if (reason.code === 'not_found' || reason.status === 404) return t('self.resourceMissing');
    if (reason.code === 'insufficient_quota') return t('self.insufficientQuota');
    if (reason.code === 'rate_limit_exceeded' || reason.status === 429) return t('self.rateLimited');
    if (reason.code === 'unpriced_model') return t('self.unpricedModel');
    if (reason.status >= 500) return t('self.temporarilyUnavailable');
  }
  return reason instanceof Error && !(reason instanceof TypeError) ? reason.message : fallback;
}

export function SelfPortal() {
  const { locale, t } = useI18n();
  const [credential, setCredential] = useState(() => sessionStorage.getItem('mtc-key') ?? '');
  const [stats, setStats] = useState<SelfStats>();
  const [credentialView, setCredentialView] = useState<KeyView>();
  const [requests, setRequests] = useState<RequestView[]>([]);
  const [requestFilters, setRequestFilters] = useState<RequestFilters>(emptyRequestFilters);
  const [appliedFilters, setAppliedFilters] = useState<RequestFilters>(emptyRequestFilters);
  const [hasOlderRequests, setHasOlderRequests] = useState(false);
  const [requestLoading, setRequestLoading] = useState(false);
  const [generations, setGenerations] = useState<GenerationJob[]>([]);
  const [conversations, setConversations] = useState<ConversationCluster[]>([]);
  const [detail, setDetail] = useState<RequestDetail>();
  const [generationDetail, setGenerationDetail] = useState<GenerationJob>();
  const [conversationDetail, setConversationDetail] = useState<ConversationDetail>();
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(false);

  async function load() {
    const value = credential.trim();
    if (!value) return;
    sessionStorage.setItem('mtc-key', value);
    setError(''); setLoading(true);
    try {
      const results = await Promise.allSettled([
        api<KeyView>('/self/v1/key', value), api<SelfStats>(statsPath(requestFilters), value),
        api<RequestView[]>(requestsPath(requestFilters), value),
        api<GenerationJob[]>('/self/v1/generations?limit=100', value),
        api<ConversationCluster[]>('/self/v1/conversations', value),
      ]);
      const failures = results.filter((result) => result.status === 'rejected');
      if (failures.length === results.length) throw failures[0].reason;
      const [nextCredential, nextStats, nextRequests, nextGenerations, nextConversations] = results;
      setCredentialView(nextCredential.status === 'fulfilled' ? nextCredential.value : undefined);
      setStats(nextStats.status === 'fulfilled' ? nextStats.value : undefined);
      const requestPage = nextRequests.status === 'fulfilled' ? nextRequests.value : [];
      setRequests(requestPage);
      setAppliedFilters(requestFilters);
      setHasOlderRequests(nextRequests.status === 'fulfilled' && requestPage.length === requestPageSize);
      setGenerations(nextGenerations.status === 'fulfilled' ? nextGenerations.value : []);
      setConversations(nextConversations.status === 'fulfilled' ? nextConversations.value : []);
      if (failures.length) setError(t('self.partialLoad', { count: failures.length }));
    } catch (reason) {
      setCredentialView(undefined); setStats(undefined); setRequests([]); setGenerations([]); setConversations([]);
      setDetail(undefined); setGenerationDetail(undefined); setConversationDetail(undefined);
      setHasOlderRequests(false);
      setError(selfErrorMessage(reason, t, t('common.requestFailed')));
    }
    finally { setLoading(false); }
  }

  async function fetchRequestPage(filters: RequestFilters, append = false) {
    const value = credential.trim();
    if (!value) return;
    const from = filters.from ? new Date(filters.from).getTime() : Number.NaN;
    const to = filters.to ? new Date(filters.to).getTime() : Number.NaN;
    if (Number.isFinite(from) && Number.isFinite(to) && from > to) {
      setError(t('self.invalidRange'));
      return;
    }
    setRequestLoading(true); setError('');
    try {
      const before = append ? requests.at(-1) : undefined;
      const [page, filteredStats] = await Promise.all([
        api<RequestView[]>(requestsPath(filters, before), value),
        api<SelfStats>(statsPath(filters), value),
      ]);
      setRequests((current) => {
        if (!append) return page;
        const known = new Set(current.map((request) => request.request_id));
        return [...current, ...page.filter((request) => !known.has(request.request_id))];
      });
      setAppliedFilters(filters);
      setStats(filteredStats);
      setHasOlderRequests(page.length === requestPageSize);
    } catch (reason) {
      setError(selfErrorMessage(reason, t, t('common.requestFailed')));
    } finally {
      setRequestLoading(false);
    }
  }

  function applyRequestFilters(event: FormEvent) {
    event.preventDefault();
    void fetchRequestPage(requestFilters);
  }

  function clearRequestFilters() {
    setRequestFilters(emptyRequestFilters);
    void fetchRequestPage(emptyRequestFilters);
  }

  function filterRequests(next: Partial<RequestFilters>) {
    const filters = { ...emptyRequestFilters, ...next };
    setRequestFilters(filters);
    void fetchRequestPage(filters);
  }

  async function select(request: RequestView) {
    try { setDetail(await api<RequestDetail>(`/self/v1/requests/${request.request_id}`, credential.trim())); }
    catch (reason) { setError(selfErrorMessage(reason, t, t('self.detailFailed'))); }
  }

  useEffect(() => { if (credential) void load(); }, []);
  return <Shell>
    <header className="hero"><div><span className="eyebrow">{t('self.eyebrow')}</span><h1>{t('self.title')}</h1><p>{t('self.subtitle')}</p></div><form className="credential" onSubmit={(event) => { event.preventDefault(); void load(); }}><input aria-label={t('self.credential')} autoComplete="current-password" type="password" value={credential} onChange={(event) => setCredential(event.target.value)} placeholder={t('self.placeholder')} /><button type="submit" disabled={loading || !credential.trim()}>{loading ? t('common.loading') : t('common.load')}</button></form></header>
    {error && <div className="notice error" role="alert">{error}</div>}
    {stats && <>
      <section className="metrics"><Metric label={t('self.balance', { currency: credentialView?.currency ?? '' })} value={credentialView?.available_balance ?? '—'} tone="positive" /><Metric label={t('traffic.total')} value={stats.summary.total_requests.toLocaleString(locale)} /><Metric label={t('traffic.success')} value={stats.summary.successful_requests.toLocaleString(locale)} tone="positive" /><Metric label={t('traffic.failure')} value={stats.summary.failed_requests.toLocaleString(locale)} tone="negative" /><Metric label={t('request.tokens')} value={(stats.summary.input_tokens + stats.summary.output_tokens).toLocaleString(locale)} /><Metric label={t('traffic.cost')} value={stats.summary.total_cost} /></section>
      {credentialView && <article className="panel key-summary"><div><span className="eyebrow">{t('self.stableCredential')}</span><h2>{credentialView.alias}</h2><code>{credentialView.key_id}</code></div><div className="policy-grid"><span><b>{t('self.credentialGeneration')}</b>{credentialView.credential_generation}</span><span><b>RPM</b>{credentialView.policy.requests_per_minute.toLocaleString(locale)}</span><span><b>TPM</b>{credentialView.policy.tokens_per_minute.toLocaleString(locale)}</span><span><b>{t('self.concurrency')}</b>{credentialView.policy.max_concurrency}</span><span><b>{t('budget.daily')}</b>{credentialView.policy.daily_budget ?? '—'}</span><span><b>{t('budget.weekly')}</b>{credentialView.policy.weekly_budget ?? '—'}</span><span><b>{t('budget.lifetime')}</b>{credentialView.policy.lifetime_budget ?? '—'}</span><span><b>{t('self.allowedModels')}</b>{credentialView.policy.allowed_models.length ? credentialView.policy.allowed_models.join(', ') : t('credentials.noModelsAllowed')}</span></div></article>}
      <section className="two-column"><article className="panel"><h2>{t('traffic.models')}</h2><Buckets values={stats.by_model} onSelect={(bucket) => filterRequests({ model: bucket.name })} /></article><article className="panel"><h2>{t('traffic.days')}</h2><Buckets values={stats.by_day} onSelect={(bucket) => { if (/^\d{4}-\d{2}-\d{2}$/.test(bucket.name)) filterRequests({ from: `${bucket.name}T00:00`, to: `${bucket.name}T23:59` }); }} /></article></section>
      {stats.errors.length > 0 && <article className="panel"><h2>{t('traffic.errors')}</h2><Buckets values={stats.errors} onSelect={(bucket) => filterRequests({ status: 'error', errorCode: bucket.name })} /></article>}
      <article className="panel self-history"><div className="panel-title"><div><h2>{t('self.recent')}</h2><p className="muted">{t('self.historyHint')}</p></div><span>{t('self.loadedRequests', { count: requests.length })}</span></div>
        <form className="self-request-filters" onSubmit={applyRequestFilters}>
          <label><span>{t('traffic.from')}</span><input type="datetime-local" value={requestFilters.from} onChange={(event) => setRequestFilters((current) => ({ ...current, from: event.target.value }))} /></label>
          <label><span>{t('traffic.to')}</span><input type="datetime-local" value={requestFilters.to} onChange={(event) => setRequestFilters((current) => ({ ...current, to: event.target.value }))} /></label>
          <label><span>{t('request.model')}</span><input value={requestFilters.model} onChange={(event) => setRequestFilters((current) => ({ ...current, model: event.target.value }))} placeholder={t('self.exactMatch')} /></label>
          <label><span>{t('request.protocol')}</span><input value={requestFilters.protocol} onChange={(event) => setRequestFilters((current) => ({ ...current, protocol: event.target.value }))} placeholder={t('self.exactMatch')} /></label>
          <label><span>{t('request.status')}</span><select value={requestFilters.status} onChange={(event) => setRequestFilters((current) => ({ ...current, status: event.target.value }))}><option value="">{t('common.all')}</option><option value="success">{t('traffic.success')}</option><option value="error">{t('traffic.failure')}</option><option value="pending">{t('common.running')}</option></select></label>
          <label><span>{t('traffic.errorCode')}</span><input value={requestFilters.errorCode} onChange={(event) => setRequestFilters((current) => ({ ...current, errorCode: event.target.value }))} placeholder={t('self.exactMatch')} /></label>
          <label><span>{t('traffic.upstreamId')}</span><input value={requestFilters.upstreamAccountId} onChange={(event) => setRequestFilters((current) => ({ ...current, upstreamAccountId: event.target.value }))} placeholder="019f…" /></label>
          <label><span>{t('traffic.routeId')}</span><input value={requestFilters.routeId} onChange={(event) => setRequestFilters((current) => ({ ...current, routeId: event.target.value }))} placeholder="019f…" /></label>
          <label><span>{t('traffic.minDuration')}</span><input type="number" min="0" value={requestFilters.minDurationMs} onChange={(event) => setRequestFilters((current) => ({ ...current, minDurationMs: event.target.value }))} /></label>
          <label><span>{t('traffic.maxDuration')}</span><input type="number" min="0" value={requestFilters.maxDurationMs} onChange={(event) => setRequestFilters((current) => ({ ...current, maxDurationMs: event.target.value }))} /></label>
          <label><span>{t('traffic.minCost')}</span><input inputMode="decimal" value={requestFilters.minCost} onChange={(event) => setRequestFilters((current) => ({ ...current, minCost: event.target.value }))} /></label>
          <label><span>{t('traffic.maxCost')}</span><input inputMode="decimal" value={requestFilters.maxCost} onChange={(event) => setRequestFilters((current) => ({ ...current, maxCost: event.target.value }))} /></label>
          <div className="filter-actions"><button type="submit" disabled={requestLoading}>{requestLoading ? t('common.loading') : t('traffic.applyFilters')}</button><button type="button" className="secondary" onClick={clearRequestFilters} disabled={requestLoading}>{t('traffic.clearFilters')}</button></div>
        </form>
        <RequestTable requests={requests} onSelect={(request) => void select(request)} />
        {hasOlderRequests && <div className="load-more"><button type="button" className="secondary" disabled={requestLoading} onClick={() => void fetchRequestPage(appliedFilters, true)}>{requestLoading ? t('common.loading') : t('traffic.loadOlder')}</button></div>}
      </article>
      <article className="panel"><div className="panel-title"><h2>{t('self.conversations')}</h2><span>{t('self.conversationHint')}</span></div><div className="conversation-list">{conversations.map((conversation) => <button type="button" className="conversation" key={conversation.cluster_id} onClick={async () => { try { setConversationDetail(await api<ConversationDetail>(`/self/v1/conversations/${conversation.cluster_id}`, credential.trim())); } catch (reason) { setError(selfErrorMessage(reason, t, t('common.requestFailed'))); } }}><span><b>{conversation.explicit_session_id ?? conversation.cluster_id.slice(0, 13)}</b><small>{new Date(conversation.updated_at).toLocaleString(locale)}</small></span><span><strong>{t('request.count', { count: conversation.request_count })}</strong>{conversation.candidate_edge_count > 0 && <em>{t('self.candidateEdges', { count: conversation.candidate_edge_count })}</em>}</span></button>)}{conversations.length === 0 && <div className="empty">{t('self.noConversations')}</div>}</div></article>
      <GenerationTable jobs={generations} onSelect={setGenerationDetail} />
    </>}
    {detail && <RequestDetailDrawer detail={detail} onClose={() => setDetail(undefined)} />}
    {generationDetail && <GenerationDrawer job={generationDetail} onClose={() => setGenerationDetail(undefined)} />}
    {conversationDetail && <ConversationDrawer detail={conversationDetail} onClose={() => setConversationDetail(undefined)} />}
  </Shell>;
}

function GenerationTable({ jobs, onSelect }: { jobs: GenerationJob[]; onSelect: (job: GenerationJob) => void }) {
  const { locale, t } = useI18n();
  return <article className="panel"><div className="panel-title"><h2>{t('self.generations')}</h2><span>{t('self.generationDrivers')}</span></div><div className="table-scroll"><table><thead><tr><th>{t('request.time')}</th><th>{t('request.model')}</th><th>{t('self.integration')}</th><th>{t('request.status')}</th><th>{t('self.units')}</th><th>{t('request.cost')}</th><th>{t('request.error')}</th></tr></thead><tbody>{jobs.map((job) => <tr key={job.job_id}><td>{new Date(job.created_at).toLocaleString(locale)}</td><td><button type="button" className="table-link" onClick={() => onSelect(job)} aria-label={t('self.openGeneration', { model: job.model })}><code>{job.model}</code></button></td><td>{job.driver}</td><td><span className={`status ${job.status === 'succeeded' ? 'ok' : job.status === 'failed' || job.status === 'cancelled' ? 'bad' : 'pending'}`}>{t(`generationStatus.${job.status}`)}</span></td><td>{job.billed_units ?? `≤ ${job.estimated_units}`}</td><td>{job.cost}</td><td>{job.error_code ?? '—'}</td></tr>)}</tbody></table>{jobs.length === 0 && <div className="empty">{t('self.noGenerations')}</div>}</div></article>;
}

function RequestDetailDrawer({ detail, onClose }: { detail: RequestDetail; onClose: () => void }) {
  const { locale, t } = useI18n();
  const successful = detail.status_code !== null && detail.status_code < 400;
  return <DrawerFrame title={detail.model} eyebrow={t('request.detail')} onClose={onClose}>
    <p className="muted break-anywhere request-identity">{detail.request_id}</p>
    <div className="request-diagnostics">
      <span><b>{t('request.time')}</b>{new Date(detail.created_at).toLocaleString(locale)}</span>
      <span><b>{t('request.status')}</b><i className={`status ${successful ? 'ok' : detail.status_code ? 'bad' : 'pending'}`}>{detail.status_code ?? t('common.running')}</i></span>
      <span><b>{t('request.protocol')}</b>{detail.protocol}</span>
      <span><b>{t('request.duration')}</b>{detail.duration_ms === null ? '—' : `${detail.duration_ms.toLocaleString(locale)} ms`}</span>
      <span><b>{t('request.tokens')}</b>{(detail.input_tokens + detail.output_tokens).toLocaleString(locale)} <small>{detail.input_tokens.toLocaleString(locale)} + {detail.output_tokens.toLocaleString(locale)}</small></span>
      <span><b>{t('request.cost')}</b>{detail.cost}</span>
      <span><b>{t('request.error')}</b>{detail.error_code ?? '—'}</span>
      <span><b>{t('self.archive')}</b>{detail.archive_complete ? t('request.archiveComplete') : t('request.archiveIncomplete')}</span>
    </div>
    <h3>{t('request.request')}</h3><pre>{JSON.stringify(detail.request_body, null, 2)}</pre><h3>{t('request.response')}</h3><pre>{JSON.stringify(detail.response_body, null, 2)}</pre>
  </DrawerFrame>;
}

function GenerationDrawer({ job, onClose }: { job: GenerationJob; onClose: () => void }) {
  const { t } = useI18n();
  return <DrawerFrame title={job.model} eyebrow={t('self.generationDetail')} onClose={onClose}><p className="muted break-anywhere">{job.job_id} · {job.driver} · {t(`generationStatus.${job.status}`)}</p><h3>{t('self.billing')}</h3><pre>{JSON.stringify({ estimated_units: job.estimated_units, billed_units: job.billed_units, cost: job.cost }, null, 2)}</pre><h3>{t('self.resultArchive')}</h3><pre>{JSON.stringify(job.result, null, 2)}</pre>{job.error_code && <><h3>{t('request.error')}</h3><pre>{job.error_code}</pre></>}</DrawerFrame>;
}

function ConversationDrawer({ detail, onClose }: { detail: ConversationDetail; onClose: () => void }) {
  const { t } = useI18n();
  return <DrawerFrame title={detail.cluster.explicit_session_id ?? t('self.inferred')} eyebrow={t('self.logicalConversation')} onClose={onClose}><p className="muted break-anywhere">{detail.cluster.cluster_id}</p><h3>{t('self.sequence')}</h3><RequestTable requests={detail.requests} /><h3>{t('self.edges')}</h3><div className="edge-list">{detail.edges.map((edge) => <div className="edge" key={`${edge.to_request_id}-${edge.relation}`}><span className={`status ${edge.relation === 'candidate' ? 'pending' : 'ok'}`}>{t(`conversationRelation.${edge.relation}`)}</span><code>{edge.from_request_id?.slice(0, 8) ?? t('self.root')} → {edge.to_request_id.slice(0, 8)}</code><b>{Math.round(edge.confidence * 100)}%</b></div>)}{detail.edges.length === 0 && <div className="empty">{t('self.singleObservation')}</div>}</div></DrawerFrame>;
}
