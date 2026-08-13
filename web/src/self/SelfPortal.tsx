import { useEffect, useState } from 'react';
import { api } from '../api';
import { Buckets, Metric, RequestTable, Shell } from '../components';
import { useI18n } from '../i18n';
import type { ConversationCluster, ConversationDetail, GenerationJob, KeyView, RequestDetail, RequestView, SelfStats } from '../types';

export function SelfPortal() {
  const { locale, t } = useI18n();
  const [credential, setCredential] = useState(() => sessionStorage.getItem('mtc-key') ?? '');
  const [stats, setStats] = useState<SelfStats>();
  const [credentialView, setCredentialView] = useState<KeyView>();
  const [requests, setRequests] = useState<RequestView[]>([]);
  const [generations, setGenerations] = useState<GenerationJob[]>([]);
  const [conversations, setConversations] = useState<ConversationCluster[]>([]);
  const [detail, setDetail] = useState<RequestDetail>();
  const [generationDetail, setGenerationDetail] = useState<GenerationJob>();
  const [conversationDetail, setConversationDetail] = useState<ConversationDetail>();
  const [error, setError] = useState('');

  async function load() {
    const value = credential.trim();
    if (!value) return;
    sessionStorage.setItem('mtc-key', value);
    setError('');
    try {
      const [nextCredential, nextStats, nextRequests, nextGenerations, nextConversations] = await Promise.all([
        api<KeyView>('/self/v1/key', value), api<SelfStats>('/self/v1/stats', value),
        api<RequestView[]>('/self/v1/requests?limit=100', value),
        api<GenerationJob[]>('/self/v1/generations?limit=100', value),
        api<ConversationCluster[]>('/self/v1/conversations', value),
      ]);
      setCredentialView(nextCredential); setStats(nextStats); setRequests(nextRequests);
      setGenerations(nextGenerations); setConversations(nextConversations);
    } catch (reason) { setError(reason instanceof Error ? reason.message : t('common.requestFailed')); }
  }

  async function select(request: RequestView) {
    try { setDetail(await api<RequestDetail>(`/self/v1/requests/${request.request_id}`, credential)); }
    catch (reason) { setError(reason instanceof Error ? reason.message : t('self.detailFailed')); }
  }

  useEffect(() => { if (credential) void load(); }, []);
  return <Shell>
    <header className="hero"><div><span className="eyebrow">CREDENTIAL OBSERVABILITY</span><h1>{t('self.title')}</h1><p>{t('self.subtitle')}</p></div><div className="credential"><input type="password" value={credential} onChange={(event) => setCredential(event.target.value)} placeholder={t('self.placeholder')} /><button onClick={() => void load()}>{t('common.load')}</button></div></header>
    {error && <div className="notice error">{error}</div>}
    {stats && <>
      <section className="metrics"><Metric label={t('self.balance', { currency: credentialView?.currency ?? '' })} value={credentialView?.available_balance ?? '—'} tone="positive" /><Metric label={t('traffic.total')} value={stats.summary.total_requests} /><Metric label={t('traffic.success')} value={stats.summary.successful_requests} tone="positive" /><Metric label={t('traffic.failure')} value={stats.summary.failed_requests} tone="negative" /><Metric label="Tokens" value={stats.summary.input_tokens + stats.summary.output_tokens} /><Metric label={t('traffic.cost')} value={stats.summary.total_cost} /></section>
      {credentialView && <article className="panel key-summary"><div><span className="eyebrow">{t('self.stableCredential')}</span><h2>{credentialView.alias}</h2><code>{credentialView.key_id}</code></div><div className="policy-grid"><span><b>generation</b>{credentialView.credential_generation}</span><span><b>RPM</b>{credentialView.policy.requests_per_minute}</span><span><b>TPM</b>{credentialView.policy.tokens_per_minute}</span><span><b>{t('self.concurrency')}</b>{credentialView.policy.max_concurrency}</span><span><b>{t('self.allowedModels')}</b>{credentialView.policy.allowed_models.length ? credentialView.policy.allowed_models.join(', ') : '*'}</span></div></article>}
      <section className="two-column"><article className="panel"><h2>{t('traffic.models')}</h2><Buckets values={stats.by_model} /></article><article className="panel"><h2>{t('traffic.days')}</h2><Buckets values={stats.by_day} /></article></section>
      {stats.errors.length > 0 && <article className="panel"><h2>{t('traffic.errors')}</h2><Buckets values={stats.errors} /></article>}
      <article className="panel"><div className="panel-title"><h2>{t('self.recent')}</h2><span>{stats.key_id}</span></div><RequestTable requests={requests} onSelect={(request) => void select(request)} /></article>
      <article className="panel"><div className="panel-title"><h2>{t('self.conversations')}</h2><span>{t('self.conversationHint')}</span></div><div className="conversation-list">{conversations.map((conversation) => <button className="conversation" key={conversation.cluster_id} onClick={async () => setConversationDetail(await api<ConversationDetail>(`/self/v1/conversations/${conversation.cluster_id}`, credential))}><span><b>{conversation.explicit_session_id ?? conversation.cluster_id.slice(0, 13)}</b><small>{new Date(conversation.updated_at).toLocaleString(locale)}</small></span><span><strong>{t('request.count', { count: conversation.request_count })}</strong>{conversation.candidate_edge_count > 0 && <em>{conversation.candidate_edge_count} candidate</em>}</span></button>)}{conversations.length === 0 && <div className="empty">{t('self.noConversations')}</div>}</div></article>
      <GenerationTable jobs={generations} onSelect={setGenerationDetail} />
    </>}
    {detail && <RequestDetailDrawer detail={detail} onClose={() => setDetail(undefined)} />}
    {generationDetail && <GenerationDrawer job={generationDetail} onClose={() => setGenerationDetail(undefined)} />}
    {conversationDetail && <ConversationDrawer detail={conversationDetail} onClose={() => setConversationDetail(undefined)} />}
  </Shell>;
}

function GenerationTable({ jobs, onSelect }: { jobs: GenerationJob[]; onSelect: (job: GenerationJob) => void }) {
  const { locale, t } = useI18n();
  return <article className="panel"><div className="panel-title"><h2>{t('self.generations')}</h2><span>Seedance · ComfyUI</span></div><div className="table-scroll"><table><thead><tr><th>{t('request.time')}</th><th>{t('request.model')}</th><th>{t('self.integration')}</th><th>{t('request.status')}</th><th>{t('self.units')}</th><th>{t('request.cost')}</th><th>{t('request.error')}</th></tr></thead><tbody>{jobs.map((job) => <tr className="clickable" key={job.job_id} onClick={() => onSelect(job)}><td>{new Date(job.created_at).toLocaleString(locale)}</td><td><code>{job.model}</code></td><td>{job.driver}</td><td><span className={`status ${job.status === 'succeeded' ? 'ok' : job.status === 'failed' || job.status === 'cancelled' ? 'bad' : 'pending'}`}>{job.status}</span></td><td>{job.billed_units ?? `≤ ${job.estimated_units}`}</td><td>{job.cost}</td><td>{job.error_code ?? '—'}</td></tr>)}</tbody></table>{jobs.length === 0 && <div className="empty">{t('self.noGenerations')}</div>}</div></article>;
}

function RequestDetailDrawer({ detail, onClose }: { detail: RequestDetail; onClose: () => void }) {
  const { t } = useI18n();
  return <div className="drawer-backdrop" onClick={onClose}><aside className="drawer" onClick={(event) => event.stopPropagation()}><button className="close" onClick={onClose}>×</button><span className="eyebrow">REQUEST DETAIL</span><h2>{detail.model}</h2><p className="muted">{detail.request_id} · {detail.archive_complete ? t('request.archiveComplete') : t('request.archiveIncomplete')}</p><h3>{t('request.request')}</h3><pre>{JSON.stringify(detail.request_body, null, 2)}</pre><h3>{t('request.response')}</h3><pre>{JSON.stringify(detail.response_body, null, 2)}</pre></aside></div>;
}

function GenerationDrawer({ job, onClose }: { job: GenerationJob; onClose: () => void }) {
  const { t } = useI18n();
  return <div className="drawer-backdrop" onClick={onClose}><aside className="drawer" onClick={(event) => event.stopPropagation()}><button className="close" onClick={onClose}>×</button><span className="eyebrow">GENERATION DETAIL</span><h2>{job.model}</h2><p className="muted">{job.job_id} · {job.driver} · {job.status}</p><h3>{t('self.billing')}</h3><pre>{JSON.stringify({ estimated_units: job.estimated_units, billed_units: job.billed_units, cost: job.cost }, null, 2)}</pre><h3>{t('self.resultArchive')}</h3><pre>{JSON.stringify(job.result, null, 2)}</pre>{job.error_code && <><h3>{t('request.error')}</h3><pre>{job.error_code}</pre></>}</aside></div>;
}

function ConversationDrawer({ detail, onClose }: { detail: ConversationDetail; onClose: () => void }) {
  const { t } = useI18n();
  return <div className="drawer-backdrop" onClick={onClose}><aside className="drawer" onClick={(event) => event.stopPropagation()}><button className="close" onClick={onClose}>×</button><span className="eyebrow">LOGICAL CONVERSATION</span><h2>{detail.cluster.explicit_session_id ?? t('self.inferred')}</h2><p className="muted">{detail.cluster.cluster_id}</p><h3>{t('self.sequence')}</h3><RequestTable requests={detail.requests} /><h3>{t('self.edges')}</h3><div className="edge-list">{detail.edges.map((edge) => <div className="edge" key={`${edge.to_request_id}-${edge.relation}`}><span className={`status ${edge.relation === 'candidate' ? 'pending' : 'ok'}`}>{edge.relation}</span><code>{edge.from_request_id?.slice(0, 8) ?? 'root'} → {edge.to_request_id.slice(0, 8)}</code><b>{Math.round(edge.confidence * 100)}%</b></div>)}{detail.edges.length === 0 && <div className="empty">{t('self.singleObservation')}</div>}</div></aside></div>;
}
