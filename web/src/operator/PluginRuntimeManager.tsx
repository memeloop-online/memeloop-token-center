import { useCallback, useEffect, useRef, useState } from 'react';
import { ApiError, api } from '../api';
import { useI18n } from '../i18n';
import type { PluginManifest } from '../types';

interface Revision { revision: number; inventory_id: string; reason: string; created_at?: number }
interface Candidate { inventory_id: string; staged: boolean; plugins: Record<string, string[]> }
interface RuntimeStatus { current: Revision | null; candidates: Candidate[] }
interface Installation {
  id: string; inventory_id: string; actor: string; status: string; packages: string[];
  review_digest: string | null; review: { plugins: PluginManifest[] } | null;
  failure_category: string | null; created_at: number; updated_at: number;
}
interface Audit { id: string; actor: string; action: string; inventory_id: string | null; revision: number | null; outcome: string; created_at: number }
interface History { installation_enabled: boolean; installations: Installation[]; revisions: Revision[]; audit: Audit[] }

const copy = {
  en: {
    title: 'Plugin installation and versions', global: 'Global administrator operations',
    unavailable: 'Runtime management is not enabled, or this credential is not a global plugin administrator.',
    loading: 'Loading runtime…', refresh: 'Refresh runtime', disabled: 'Installation is disabled. Configure host signing trust and shared inventory storage to enable it.',
    scope: 'Each inventory is a complete plugin set. Keep required policies and providers. Installation alone does not activate code.',
    inventory: 'New inventory ID', packages: 'Digest-pinned OCI references (one per line)', install: 'Install for review',
    tasks: 'Installation tasks', review: 'Review manifest and requested capabilities', approve: 'Approve this exact inventory',
    warning: 'Approval grants every displayed capability and contract. Review endpoints, schemas and provider contributions before confirming.',
    retry: 'Retry installation', candidates: 'Available inventories', stage: 'Validate inventory', publish: 'Publish inventory',
    current: 'Current revision', baseline: 'Startup baseline (no published revision)', history: 'Version history', rollback: 'Roll back to this version',
    audit: 'Operation audit', older: 'Load older versions', olderAudit: 'Load older audit records', empty: 'No records yet.', done: 'Operation completed.',
    failed: 'Operation failed. Refresh to check the durable result before retrying.', confirm: 'Confirm global activation',
    pending: 'Working…', staged: 'Validated', unstaged: 'Not yet validated',
  },
  zh: {
    title: '插件安装与版本管理', global: '全局管理员操作',
    unavailable: '运行时管理未启用，或当前凭据不是全局插件管理员。',
    loading: '正在读取运行时…', refresh: '刷新运行时', disabled: '安装功能未启用。请配置主机签名信任策略和共享库存存储。',
    scope: '每个库存必须包含完整插件集，请保留需要的策略和供应商。安装本身不会激活代码。',
    inventory: '新库存 ID', packages: '固定摘要的 OCI 引用（每行一个）', install: '安装并待审阅',
    tasks: '安装任务', review: '审阅清单与请求能力', approve: '批准此精确库存',
    warning: '批准会授予所列全部能力与契约。请确认端点、配置结构和供应商贡献后再批准。',
    retry: '重试安装', candidates: '可用库存', stage: '校验库存', publish: '发布库存',
    current: '当前版本', baseline: '启动基线（尚未发布版本）', history: '版本历史', rollback: '回退到此版本',
    audit: '操作审计', older: '读取更早版本', olderAudit: '读取更早审计记录', empty: '暂无记录。', done: '操作已完成。',
    failed: '操作失败，请刷新确认持久化结果后重试。', confirm: '确认全局激活',
    pending: '处理中…', staged: '已校验', unstaged: '尚未校验',
  },
};

export function PluginRuntimeManager({ token, onPublished }: { token: string; onPublished: () => Promise<void> }) {
  const { locale } = useI18n();
  const text = locale.startsWith('zh') ? copy.zh : copy.en;
  const [status, setStatus] = useState<RuntimeStatus>();
  const [history, setHistory] = useState<History>();
  const [unavailable, setUnavailable] = useState(false);
  const [error, setError] = useState('');
  const [message, setMessage] = useState('');
  const [busy, setBusy] = useState(false);
  const [inventory, setInventory] = useState('');
  const [packages, setPackages] = useState('');
  const [confirmation, setConfirmation] = useState(false);
  const [approvals, setApprovals] = useState<Record<string, boolean>>({});
  const [reviews, setReviews] = useState<Record<string, Installation>>({});
  const generation = useRef(0);
  const alive = useRef(true);
  const keys = useRef(new Map<string, string>());

  useEffect(() => { alive.current = true; return () => { alive.current = false; generation.current++; }; }, []);
  const load = useCallback(async () => {
    const request = ++generation.current;
    try {
      const [nextStatus, nextHistory] = await Promise.all([
        api<RuntimeStatus>('/internal/v1/plugin-runtime', token),
        api<History>('/internal/v1/plugin-runtime/history', token),
      ]);
      if (!alive.current || request !== generation.current) return;
      setStatus(nextStatus); setHistory(nextHistory); setUnavailable(false);
    } catch (reason) {
      if (!alive.current || request !== generation.current) return;
      if (reason instanceof ApiError && [403, 404].includes(reason.status)) setUnavailable(true);
      else setError(reason instanceof Error ? reason.message : 'Request failed');
    }
  }, [token]);
  useEffect(() => { void load(); }, [load]);
  const installing = history?.installations.some((job) => job.status === 'installing') ?? false;
  useEffect(() => {
    if (!installing) return;
    const timer = window.setInterval(() => { void load(); }, 3000);
    return () => window.clearInterval(timer);
  }, [installing, load]);

  async function mutate(path: string, body: unknown, activates = false) {
    if (busy) return;
    setBusy(true); setError(''); setMessage('');
    const encoded = JSON.stringify(body);
    const identity = `${path}\0${encoded}`;
    const key = keys.current.get(identity) ?? crypto.randomUUID();
    keys.current.set(identity, key);
    try {
      await api(path, token, { method: 'POST', body: encoded, headers: { 'Idempotency-Key': key } });
      if (!alive.current) return;
      setMessage(text.done); setConfirmation(false); setApprovals({});
      await load();
      if (activates) await onPublished();
    } catch (reason) {
      if (alive.current) { setError(`${text.failed} ${reason instanceof Error ? reason.message : ''}`); await load(); }
    } finally { if (alive.current) setBusy(false); }
  }

  async function older() {
    const last = history?.revisions.at(-1);
    if (!last || busy) return;
    setBusy(true);
    try {
      const page = await api<History>(`/internal/v1/plugin-runtime/history?before_revision=${last.revision}`, token);
      if (alive.current) setHistory((current) => current ? { ...current, revisions: [...current.revisions, ...page.revisions.filter((item) => !current.revisions.some((old) => old.revision === item.revision))] } : page);
    } catch (reason) { if (alive.current) setError(reason instanceof Error ? reason.message : text.failed); }
    finally { if (alive.current) setBusy(false); }
  }

  async function olderAudit() {
    const last = history?.audit.at(-1);
    if (!last || busy) return;
    setBusy(true);
    try {
      const page = await api<History>(`/internal/v1/plugin-runtime/history?before_audit_id=${encodeURIComponent(last.id)}`, token);
      if (alive.current) setHistory((current) => current ? { ...current, audit: [...current.audit, ...page.audit.filter((item) => !current.audit.some((old) => old.id === item.id))] } : page);
    } catch (reason) { if (alive.current) setError(reason instanceof Error ? reason.message : text.failed); }
    finally { if (alive.current) setBusy(false); }
  }

  async function review(job: Installation) {
    try {
      const record = await api<Installation>(`/internal/v1/plugin-runtime/installations/${encodeURIComponent(job.id)}`, token);
      if (alive.current) { setReviews((old) => ({ ...old, [job.id]: record })); setApprovals((old) => ({ ...old, [job.id]: false })); }
    } catch (reason) { if (alive.current) setError(reason instanceof Error ? reason.message : text.failed); }
  }

  return <article className="panel" aria-label={text.title}>
    <div className="panel-title"><div><h2>{text.title}</h2><p className="muted">{text.global}</p></div><button type="button" className="secondary" disabled={busy} onClick={() => void load()}>{text.refresh}</button></div>
    {error && <p className="notice error" role="alert">{error}</p>}
    {message && <p className="notice" role="status">{message}</p>}
    {unavailable ? <p className="muted">{text.unavailable}</p> : !status || !history ? <p>{text.loading}</p> : <>
      <p>{text.current}: {status.current ? `${status.current.revision} · ${status.current.inventory_id}` : text.baseline}</p>
      <p className="muted">{text.scope}</p>
      {!history.installation_enabled ? <p className="notice">{text.disabled}</p> : <form onSubmit={(event) => { event.preventDefault(); void mutate('/internal/v1/plugin-runtime/installations', { inventory_id: inventory.trim(), packages: packages.split('\n').map((line) => line.trim()).filter(Boolean) }); }}>
        <label>{text.inventory}<input required pattern="[A-Za-z0-9_-]{1,64}" value={inventory} onChange={(event) => setInventory(event.target.value)} disabled={busy} /></label>
        <label>{text.packages}<textarea required rows={3} value={packages} onChange={(event) => setPackages(event.target.value)} disabled={busy} placeholder="ghcr.io/example/plugin@sha256:…" /></label>
        <button disabled={busy} type="submit">{busy ? text.pending : text.install}</button>
      </form>}
      <h3>{text.tasks}</h3>
      {history.installations.length === 0 && <p className="muted">{text.empty}</p>}
      {history.installations.map((job) => <section className="managed-resource" key={job.id}>
        <b>{job.inventory_id}</b> <span className="pill">{job.status}</span><p className="muted">{new Date(job.updated_at).toLocaleString(locale)} · {job.actor}</p>
        {job.failure_category && <p role="alert">{job.failure_category}</p>}
        {job.review_digest && <button type="button" className="secondary" onClick={() => void review(job)}>{text.review}</button>}
        {reviews[job.id]?.review && <details open><summary>{text.review}</summary><pre style={{ maxHeight: '24rem', overflow: 'auto', whiteSpace: 'pre-wrap' }}>{JSON.stringify(reviews[job.id].review, null, 2)}</pre></details>}
        {job.status === 'review' && job.review_digest && <><p>{text.warning}</p><label><input type="checkbox" disabled={!reviews[job.id]?.review || reviews[job.id].review_digest !== job.review_digest} checked={approvals[job.id] === true} onChange={(event) => setApprovals((old) => ({ ...old, [job.id]: event.target.checked }))} />{text.approve}</label><button type="button" disabled={busy || !approvals[job.id] || reviews[job.id]?.review_digest !== job.review_digest} onClick={() => void mutate(`/internal/v1/plugin-runtime/installations/${encodeURIComponent(job.id)}/approve`, { review_digest: reviews[job.id].review_digest })}>{text.approve}</button></>}
        {['failed', 'interrupted'].includes(job.status) && <button type="button" disabled={busy} onClick={() => void mutate(`/internal/v1/plugin-runtime/installations/${encodeURIComponent(job.id)}/retry`, {})}>{text.retry}</button>}
      </section>)}
      <h3>{text.candidates}</h3>
      <label><input type="checkbox" checked={confirmation} onChange={(event) => setConfirmation(event.target.checked)} />{text.confirm}</label>
      {status.candidates.map((candidate) => <div className="managed-resource" key={candidate.inventory_id}>
        <b>{candidate.inventory_id}</b><span className="pill">{candidate.staged ? text.staged : text.unstaged}</span>
        <p>{Object.entries(candidate.plugins).map(([id, versions]) => `${id}: ${versions.join(', ')}`).join(' · ')}</p>
        <button type="button" className="secondary" disabled={busy} onClick={() => void mutate('/internal/v1/plugin-runtime/candidates', { inventory_id: candidate.inventory_id })}>{text.stage}</button>
        <button type="button" disabled={busy || !confirmation || candidate.inventory_id === status.current?.inventory_id} onClick={() => void mutate('/internal/v1/plugin-runtime/publish', { inventory_id: candidate.inventory_id, expected_revision: status.current?.revision ?? 0 }, true)}>{text.publish}</button>
      </div>)}
      <h3>{text.history}</h3>
      {history.revisions.length === 0 && <p className="muted">{text.empty}</p>}
      {history.revisions.map((revision) => <div className="managed-resource" key={revision.revision}>
        <b>{revision.revision} · {revision.inventory_id}</b> <span>{revision.reason}</span>
        <button type="button" className="secondary" disabled={busy || !confirmation || !status.current || revision.revision >= status.current.revision || revision.inventory_id === status.current.inventory_id} onClick={() => void mutate('/internal/v1/plugin-runtime/rollback', { target_revision: revision.revision, expected_revision: status.current?.revision }, true)}>{text.rollback}</button>
      </div>)}
      {history.revisions.length > 0 && <button type="button" className="secondary" disabled={busy} onClick={() => void older()}>{text.older}</button>}
      <h3>{text.audit}</h3>
      {history.audit.length === 0 ? <p className="muted">{text.empty}</p> : <><ul>{history.audit.map((entry) => <li key={entry.id}>{new Date(entry.created_at).toLocaleString(locale)} · {entry.actor} · {entry.action} · {entry.inventory_id ?? '—'} · {entry.revision ?? '—'} · {entry.outcome}</li>)}</ul><button type="button" className="secondary" disabled={busy} onClick={() => void olderAudit()}>{text.olderAudit}</button></>}
    </>}
  </article>;
}
