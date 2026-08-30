import { useEffect, useRef, useState } from 'react';
import { ApiError, api } from '../api';
import { DrawerFrame } from '../components';
import { formatCurrency, formatNumber } from '../format';
import { useI18n } from '../i18n';
import type { GenerationAsset, OperatorGenerationJob } from '../types';

function tenantQuery(tenant: string) {
  const query = new URLSearchParams();
  if (tenant) query.set('tenant_external_id', tenant);
  return query.toString() ? '?' + query : '';
}

function canCancel(job: OperatorGenerationJob) {
  return job.status === 'queued' || job.status === 'running';
}

export function GenerationWorkspace({ token, tenant }: { token: string; tenant: string }) {
  const { locale, t } = useI18n();
  const [jobs, setJobs] = useState<OperatorGenerationJob[]>([]);
  const [detail, setDetail] = useState<OperatorGenerationJob>();
  const [loading, setLoading] = useState(false);
  const [busy, setBusy] = useState('');
  const [error, setError] = useState('');
  const [message, setMessage] = useState('');
  const loadSequence = useRef(0);
  const detailSequence = useRef(0);
  const scope = useRef({ token, tenant });
  scope.current = { token, tenant };

  const load = async () => {
    const sequence = ++loadSequence.current;
    const loadToken = token.trim(); const loadTenant = tenant;
    if (!loadToken) { setJobs([]); setDetail(undefined); return; }
    setLoading(true); setError('');
    try {
      const next = await api<OperatorGenerationJob[]>('/internal/v1/generations' + tenantQuery(loadTenant), loadToken);
      if (sequence !== loadSequence.current || scope.current.token.trim() !== loadToken || scope.current.tenant !== loadTenant) return;
      setJobs(next);
    } catch (reason) {
      if (sequence !== loadSequence.current || scope.current.token.trim() !== loadToken || scope.current.tenant !== loadTenant) return;
      setJobs([]);
      setError(reason instanceof Error ? reason.message : t('generations.loadFailed'));
    } finally {
      if (sequence === loadSequence.current && scope.current.token.trim() === loadToken && scope.current.tenant === loadTenant) setLoading(false);
    }
  };

  useEffect(() => {
    loadSequence.current += 1; detailSequence.current += 1;
    setJobs([]); setDetail(undefined); setBusy(''); setLoading(false); setMessage(''); setError('');
    void load();
  }, [token, tenant]);

  const select = async (job: OperatorGenerationJob) => {
    const sequence = ++detailSequence.current;
    const selectToken = token.trim(); const selectTenant = tenant;
    setError('');
    try {
      const next = await api<OperatorGenerationJob>('/internal/v1/generations/' + job.job_id + tenantQuery(job.tenant_external_id), selectToken);
      if (sequence === detailSequence.current && scope.current.token.trim() === selectToken && scope.current.tenant === selectTenant) setDetail(next);
    } catch (reason) {
      if (sequence === detailSequence.current && scope.current.token.trim() === selectToken && scope.current.tenant === selectTenant) setError(reason instanceof Error ? reason.message : t('generations.detailFailed'));
    }
  };

  const cancel = async (job: OperatorGenerationJob) => {
    if (!tenant || !canCancel(job) || !window.confirm(t('generations.confirmCancel', { model: job.model }))) return;
    const cancelToken = token.trim(); const cancelTenant = tenant;
    setBusy(job.job_id); setError(''); setMessage('');
    try {
      const cancelled = await api<OperatorGenerationJob>('/internal/v1/generations/' + job.job_id + tenantQuery(cancelTenant), cancelToken, { method: 'DELETE' });
      if (scope.current.token.trim() !== cancelToken || scope.current.tenant !== cancelTenant) return;
      setJobs((current) => current.map((value) => value.job_id === cancelled.job_id ? cancelled : value));
      setDetail((current) => current?.job_id === cancelled.job_id ? cancelled : current);
      setMessage(t('generations.cancelRequested'));
    } catch (reason) {
      if (scope.current.token.trim() === cancelToken && scope.current.tenant === cancelTenant) setError(reason instanceof Error ? reason.message : t('generations.cancelFailed'));
    } finally {
      if (scope.current.token.trim() === cancelToken && scope.current.tenant === cancelTenant) setBusy('');
    }
  };

  const download = async (job: OperatorGenerationJob, asset: GenerationAsset) => {
    const downloadToken = token.trim(); const downloadTenant = tenant;
    try {
      const response = await fetch('/internal/v1/generations/' + job.job_id + '/assets/' + asset.asset_id + tenantQuery(job.tenant_external_id), {
        headers: { Authorization: 'Bearer ' + downloadToken },
      });
      if (scope.current.token.trim() !== downloadToken || scope.current.tenant !== downloadTenant) return;
      if (!response.ok) throw new ApiError('HTTP ' + response.status, response.status);
      const objectUrl = URL.createObjectURL(await response.blob());
      const link = document.createElement('a');
      link.href = objectUrl; link.download = asset.filename; link.click();
      URL.revokeObjectURL(objectUrl);
    } catch (reason) {
      if (scope.current.token.trim() === downloadToken && scope.current.tenant === downloadTenant) setError(reason instanceof Error ? reason.message : t('generations.assetFailed'));
    }
  };

  return <>
    {!tenant && <div className="notice warning" role="status">{t('generations.allTenantsReadOnly')}</div>}
    {error && <div className="notice error" role="alert">{error}</div>}
    {message && <div className="notice success" role="status">{message}</div>}
    <article className="panel operator-generations">
      <div className="panel-title"><div><h2>{t('generations.title')}</h2><p className="muted">{t('generations.description')}</p></div><div className="row-actions"><span>{formatNumber(jobs.length, locale)}</span><button type="button" className="secondary" disabled={loading || !token.trim()} onClick={() => void load()}>{loading ? t('common.loading') : t('usage.refresh')}</button></div></div>
      {jobs.length === 0 ? <div className="empty">{loading ? t('common.loading') : t('generations.empty')}</div> : <div className="table-scroll"><table>
        <thead><tr><th>{t('request.time')}</th><th>{t('operator.tenant')}</th><th>{t('generations.credential')}</th><th>{t('request.model')}</th><th>{t('generations.driver')}</th><th>{t('request.status')}</th><th>{t('generations.units')}</th><th>{t('request.cost')}</th><th>{t('request.actions')}</th></tr></thead>
        <tbody>{jobs.map((job) => <tr key={job.job_id}>
          <td>{new Date(job.created_at).toLocaleString(locale === 'en' ? 'en-US' : 'zh-CN')}</td>
          <td>{job.tenant_external_id}</td><td><button type="button" className="table-link" onClick={() => void select(job)}>{job.key_alias}</button><small className="break-anywhere">{job.key_id}</small></td>
          <td><code>{job.model}</code></td><td>{job.driver}</td><td><span className={'status ' + (job.status === 'succeeded' ? 'ok' : job.status === 'failed' || job.status === 'cancelled' ? 'bad' : 'pending')}>{t('status.' + job.status)}</span></td>
          <td>{formatNumber(job.billed_units ?? job.estimated_units, locale)} · {t('billingUnit.' + job.billing_unit)}</td>
          <td>{formatCurrency(job.cost, job.currency, locale)}</td>
          <td><div className="row-actions"><button type="button" className="secondary" onClick={() => void select(job)}>{t('generations.details')}</button><button type="button" className="danger" disabled={!tenant || !canCancel(job) || busy === job.job_id} title={!tenant ? t('generations.selectTenantToCancel') : undefined} onClick={() => void cancel(job)}>{t('common.cancel')}</button></div></td>
        </tr>)}</tbody>
      </table></div>}
    </article>
    {detail && <DrawerFrame title={detail.model} eyebrow={t('generations.detailTitle')} onClose={() => setDetail(undefined)}>
      <p className="muted break-anywhere">{detail.job_id} · {detail.tenant_external_id} · {detail.key_alias}</p>
      <h3>{t('request.status')}</h3><pre>{detail.status}</pre>
      <h3>{t('generations.units')}</h3><pre>{JSON.stringify({ estimated: detail.estimated_units, billed: detail.billed_units, billing_unit: detail.billing_unit, cost: detail.cost, currency: detail.currency }, null, 2)}</pre>
      <h3>{t('request.error')}</h3><pre>{detail.error_code ?? t('common.none')}</pre>
      <h3>{t('generations.result')}</h3><pre>{JSON.stringify(detail.result, null, 2)}</pre>
      <h3>{t('generations.assets')}</h3>{detail.assets.length === 0 ? <p>{t('common.none')}</p> : <div className="row-actions">{detail.assets.map((asset) => <button type="button" className="secondary" key={asset.asset_id} onClick={() => void download(detail, asset)}>{asset.filename} · {formatNumber(asset.size_bytes, locale)} B</button>)}</div>}
      {tenant && canCancel(detail) && <button type="button" className="danger" disabled={busy === detail.job_id} onClick={() => void cancel(detail)}>{t('common.cancel')}</button>}
    </DrawerFrame>}
  </>;
}
