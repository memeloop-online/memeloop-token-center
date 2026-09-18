import { useConfirmDialog } from '../useConfirmDialog';
import { useEffect, useRef, useState } from 'react';
import { ApiError, api } from '../api';
import { DrawerFrame } from '../components';
import { Button, DetailTooltip } from '../design-system';
import { formatCurrency, formatNumber } from '../format';
import { useI18n } from '../i18n';
import { tenantDisplayName } from '../tenantDisplayName';
import type { GenerationAsset, OperatorGenerationJob } from '../types';
import './generationWorkspace.css';
import { credentialDisplayName } from '../identityPresentation.js';

function tenantQuery(tenant: string) {
  const query = new URLSearchParams();
  if (tenant) query.set('tenant_external_id', tenant);
  return query.toString() ? '?' + query : '';
}

function canCancel(job: OperatorGenerationJob) {
  return job.status === 'queued' || job.status === 'running';
}

function statusTone(status: OperatorGenerationJob['status']) {
  if (status === 'succeeded') return 'ok';
  if (status === 'failed' || status === 'cancelled') return 'bad';
  return 'pending';
}

/** `tenant` scopes reads; `writeTenant` is always an explicit mutation target. */
export function GenerationWorkspace({ token, tenant, writeTenant = tenant }: { token: string; tenant: string; writeTenant?: string }) {
  const { locale, t } = useI18n();
  const { confirm, confirmationDialog } = useConfirmDialog([token, tenant, writeTenant]);
  const [jobs, setJobs] = useState<OperatorGenerationJob[]>([]);
  const [detail, setDetail] = useState<OperatorGenerationJob>();
  const [loading, setLoading] = useState(false);
  const [busy, setBusy] = useState('');
  const [error, setError] = useState('');
  const [message, setMessage] = useState('');
  const loadSequence = useRef(0);
  const detailSequence = useRef(0);
  const scope = useRef({ token, tenant, writeTenant });
  scope.current = { token, tenant, writeTenant };

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
  }, [token, tenant, writeTenant]);

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
    if (!writeTenant || job.tenant_external_id !== writeTenant || !canCancel(job) || !await confirm(t('generations.confirmCancel', { model: job.model }))) return;
    const cancelToken = token.trim(); const cancelTenant = writeTenant;
    setBusy(job.job_id); setError(''); setMessage('');
    try {
      const cancelled = await api<OperatorGenerationJob>('/internal/v1/generations/' + job.job_id + tenantQuery(cancelTenant), cancelToken, { method: 'DELETE' });
      if (scope.current.token.trim() !== cancelToken || scope.current.writeTenant !== cancelTenant) return;
      setJobs((current) => current.map((value) => value.job_id === cancelled.job_id ? cancelled : value));
      setDetail((current) => current?.job_id === cancelled.job_id ? cancelled : current);
      setMessage(t('generations.cancelRequested'));
    } catch (reason) {
      if (scope.current.token.trim() === cancelToken && scope.current.writeTenant === cancelTenant) setError(reason instanceof Error ? reason.message : t('generations.cancelFailed'));
    } finally {
      if (scope.current.token.trim() === cancelToken && scope.current.writeTenant === cancelTenant) setBusy('');
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

  const canManage = (job: OperatorGenerationJob) => Boolean(writeTenant) && job.tenant_external_id === writeTenant;

  return <>{confirmationDialog}
    {!detail && error && <div className="notice error" role="alert">{error}</div>}
    {!detail && message && <div className="notice success" role="status">{message}</div>}
    <article className="panel operator-generations">
      <div className="panel-title"><div><h2>{t('generations.title')}</h2><p className="muted">{t('generations.description')}</p></div><div className="row-actions"><span>{formatNumber(jobs.length, locale)}</span><Button appearance="secondary" disabled={loading || !token.trim()} onClick={() => void load()}>{loading ? t('common.loading') : t('usage.refresh')}</Button></div></div>
      {jobs.length === 0 ? <div className="empty">{loading ? t('common.loading') : t('generations.empty')}</div> : <div className="table-scroll generation-table-scroll"><table className="generation-table">
        <thead><tr><th>{t('request.time')}</th><th>{t('operator.tenant')}</th><th>{t('generations.credential')}</th><th>{t('request.model')}</th><th>{t('generations.driver')}</th><th>{t('request.status')}</th><th>{t('generations.units')}</th><th>{t('request.cost')}</th><th>{t('request.actions')}</th></tr></thead>
        <tbody>{jobs.map((job) => <tr key={job.job_id}>
          <td className="generation-time-cell" data-label={t('request.time')}>{new Date(job.created_at).toLocaleString(locale === 'en' ? 'en-US' : 'zh-CN')}</td>
          <td className="generation-tenant-cell" data-label={t('operator.tenant')}>{tenantDisplayName(job.tenant_external_id, locale)}</td><td className="generation-credential-cell" data-label={t('generations.credential')}><DetailTooltip content={job.key_id}><button type="button" className="table-link" onClick={() => void select(job)}>{credentialDisplayName(job.key_alias, t)}</button></DetailTooltip></td>
          <td className="generation-model-cell"><code>{job.model}</code></td><td className="generation-driver-cell" data-label={t('generations.driver')}>{job.driver}</td><td className="generation-status-cell" data-label={t('request.status')}><span className={'status ' + statusTone(job.status)}>{t('generations.status.' + job.status)}</span></td>
          <td className="generation-units-cell" data-label={t('generations.units')}>{formatNumber(job.billed_units ?? job.estimated_units, locale)} · {t('billingUnit.' + job.billing_unit)}</td>
          <td className="generation-cost-cell" data-label={t('request.cost')}>{formatCurrency(job.cost, job.currency, locale)}</td>
          <td className="generation-actions-cell"><div className="row-actions"><Button appearance="secondary" size="small" onClick={() => void select(job)}>{t('generations.details')}</Button>{canManage(job) && canCancel(job) && <button type="button" className="danger" disabled={busy === job.job_id} onClick={() => void cancel(job)}>{t('common.cancel')}</button>}</div></td>
        </tr>)}</tbody>
      </table></div>}
    </article>
    {detail && <DrawerFrame title={detail.model} eyebrow={t('generations.detailTitle')} onClose={() => setDetail(undefined)}>
      {error && <div className="notice error" role="alert">{error}</div>}
      {message && <div className="notice success" role="status">{message}</div>}
      <p className="muted break-anywhere">{detail.job_id} · {tenantDisplayName(detail.tenant_external_id, locale)} · <DetailTooltip content={detail.key_id}><span tabIndex={0}>{credentialDisplayName(detail.key_alias, t)}</span></DetailTooltip></p>
      <h3>{t('request.status')}</h3><p><span className={'status ' + statusTone(detail.status)}>{t('generations.status.' + detail.status)}</span> <code>{detail.status}</code></p>
      <h3>{t('generations.units')}</h3><pre>{JSON.stringify({ estimated: detail.estimated_units, billed: detail.billed_units, billing_unit: detail.billing_unit, cost: detail.cost, currency: detail.currency }, null, 2)}</pre>
      <h3>{t('request.error')}</h3><pre>{detail.error_code ?? t('common.none')}</pre>
      <h3>{t('generations.result')}</h3><pre>{JSON.stringify(detail.result, null, 2)}</pre>
      <h3>{t('generations.assets')}</h3>{detail.assets.length === 0 ? <p>{t('common.none')}</p> : <div className="row-actions">{detail.assets.map((asset) => <button type="button" className="secondary" key={asset.asset_id} onClick={() => void download(detail, asset)}>{asset.filename} · {formatNumber(asset.size_bytes, locale)} B</button>)}</div>}
      {canManage(detail) && canCancel(detail) && <button type="button" className="danger" disabled={busy === detail.job_id} onClick={() => void cancel(detail)}>{t('common.cancel')}</button>}
    </DrawerFrame>}
  </>;
}
