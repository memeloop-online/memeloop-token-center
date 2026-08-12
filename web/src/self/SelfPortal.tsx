import { useEffect, useState } from 'react';
import { api } from '../api';
import { Buckets, Metric, RequestTable, Shell } from '../components';
import type { GenerationJob, RequestView, SelfStats } from '../types';

interface RequestDetail extends RequestView {
  request_body: unknown;
  response_body: unknown;
  archive_complete: boolean;
}

export function SelfPortal() {
  const [key, setKey] = useState(() => sessionStorage.getItem('mtc-key') ?? '');
  const [stats, setStats] = useState<SelfStats>();
  const [requests, setRequests] = useState<RequestView[]>([]);
  const [generations, setGenerations] = useState<GenerationJob[]>([]);
  const [detail, setDetail] = useState<RequestDetail>();
  const [generationDetail, setGenerationDetail] = useState<GenerationJob>();
  const [error, setError] = useState('');

  async function load() {
    if (!key.trim()) return;
    sessionStorage.setItem('mtc-key', key.trim());
    setError('');
    try {
      const [nextStats, nextRequests, nextGenerations] = await Promise.all([
        api<SelfStats>('/self/v1/stats', key),
        api<RequestView[]>('/self/v1/requests?limit=100', key),
        api<GenerationJob[]>('/self/v1/generations?limit=100', key),
      ]);
      setStats(nextStats);
      setRequests(nextRequests);
      setGenerations(nextGenerations);
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : '请求失败');
    }
  }

  async function select(request: RequestView) {
    try {
      setDetail(await api<RequestDetail>(`/self/v1/requests/${request.request_id}`, key));
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : '无法读取请求详情');
    }
  }

  useEffect(() => { if (key) void load(); }, []);

  return (
    <Shell>
      <header className="hero">
        <div><span className="eyebrow">KEY OBSERVABILITY</span><h1>请求与用量</h1><p>凭当前 API key 只读查看它的稳定身份、历史、错误和逻辑会话。</p></div>
        <div className="credential"><input type="password" value={key} onChange={(event) => setKey(event.target.value)} placeholder="mtc_…" /><button onClick={() => void load()}>载入</button></div>
      </header>
      {error && <div className="notice error">{error}</div>}
      {stats && <>
        <section className="metrics">
          <Metric label="总请求" value={stats.summary.total_requests} />
          <Metric label="成功" value={stats.summary.successful_requests} tone="positive" />
          <Metric label="失败" value={stats.summary.failed_requests} tone="negative" />
          <Metric label="Tokens" value={stats.summary.input_tokens + stats.summary.output_tokens} />
          <Metric label="总费用" value={stats.summary.total_cost} />
        </section>
        <section className="two-column"><article className="panel"><h2>模型分布</h2><Buckets values={stats.by_model} /></article><article className="panel"><h2>每日趋势</h2><Buckets values={stats.by_day} /></article></section>
        {stats.errors.length > 0 && <article className="panel"><h2>错误分布</h2><Buckets values={stats.errors} /></article>}
        <article className="panel"><div className="panel-title"><h2>最近请求</h2><span>{stats.key_id}</span></div><RequestTable requests={requests} onSelect={(request) => void select(request)} /></article>
        <article className="panel"><div className="panel-title"><h2>多模态生成任务</h2><span>Seedance · ComfyUI</span></div><div className="table-scroll"><table><thead><tr><th>时间</th><th>模型</th><th>接入</th><th>状态</th><th>计费单位</th><th>费用</th><th>错误</th></tr></thead><tbody>{generations.map((job) => <tr className="clickable" key={job.job_id} onClick={() => setGenerationDetail(job)}><td>{new Date(job.created_at).toLocaleString()}</td><td><code>{job.model}</code></td><td>{job.driver}</td><td><span className={`status ${job.status === 'succeeded' ? 'ok' : job.status === 'failed' || job.status === 'cancelled' ? 'bad' : 'pending'}`}>{job.status}</span></td><td>{job.billed_units ?? `≤ ${job.estimated_units}`}</td><td>{job.cost}</td><td>{job.error_code ?? '—'}</td></tr>)}</tbody></table>{generations.length === 0 && <div className="empty">暂无生成任务</div>}</div></article>
      </>}
      {detail && <div className="drawer-backdrop" onClick={() => setDetail(undefined)}><aside className="drawer" onClick={(event) => event.stopPropagation()}><button className="close" onClick={() => setDetail(undefined)}>×</button><span className="eyebrow">REQUEST DETAIL</span><h2>{detail.model}</h2><p className="muted">{detail.request_id} · {detail.archive_complete ? '归档完整' : '存在归档缺口'}</p><h3>请求</h3><pre>{JSON.stringify(detail.request_body, null, 2)}</pre><h3>响应</h3><pre>{JSON.stringify(detail.response_body, null, 2)}</pre></aside></div>}
      {generationDetail && <div className="drawer-backdrop" onClick={() => setGenerationDetail(undefined)}><aside className="drawer" onClick={(event) => event.stopPropagation()}><button className="close" onClick={() => setGenerationDetail(undefined)}>×</button><span className="eyebrow">GENERATION DETAIL</span><h2>{generationDetail.model}</h2><p className="muted">{generationDetail.job_id} · {generationDetail.driver} · {generationDetail.status}</p><h3>计费</h3><pre>{JSON.stringify({ estimated_units: generationDetail.estimated_units, billed_units: generationDetail.billed_units, cost: generationDetail.cost }, null, 2)}</pre><h3>结果与归档</h3><pre>{JSON.stringify(generationDetail.result, null, 2)}</pre>{generationDetail.error_code && <><h3>错误</h3><pre>{generationDetail.error_code}</pre></>}</aside></div>}
    </Shell>
  );
}
