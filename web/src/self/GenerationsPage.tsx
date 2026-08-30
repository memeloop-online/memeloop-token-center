import { useCallback, useEffect, useRef, useState } from 'react';
import { ApiError, api } from '../api';
import { formatCurrency, formatNumber } from '../format';
import { useI18n } from '../i18n';
import type { GenerationAsset, GenerationJob, KeyView } from '../types';
import { selfErrorMessage } from './errors';
import { GenerationDrawer } from './SelfDrawers';
import { GenerationActionRegistry, startCompletionPolling } from './generationConcurrency';

export function GenerationsPage({ credential, credentialView, onError }: {
  credential: string;
  credentialView: KeyView;
  onError: (message: string) => void;
}) {
  const { locale, t } = useI18n();
  const [jobs, setJobs] = useState<GenerationJob[]>([]);
  const [selected, setSelected] = useState<GenerationJob>();
  const [loading, setLoading] = useState(true);
  const [message, setMessage] = useState('');
  const [cancellingIds, setCancellingIds] = useState<Set<string>>(() => new Set());
  const scopeGeneration = useRef(0);
  const refreshSequence = useRef(0);
  const refreshInFlight = useRef(false);
  const refreshController = useRef<AbortController | undefined>(undefined);
  const actionControllers = useRef(new GenerationActionRegistry());

  const refresh = useCallback(async () => {
    if (refreshInFlight.current) return;
    const current = ++refreshSequence.current;
    const scope = scopeGeneration.current;
    const controller = new AbortController();
    refreshController.current = controller;
    refreshInFlight.current = true;
    try {
      const response = await api<GenerationJob[]>('/self/v1/generations?limit=100', credential, { signal: controller.signal });
      if (scope !== scopeGeneration.current || current !== refreshSequence.current || controller.signal.aborted) return;
      setJobs(response);
      setSelected((job) => job ? response.find((candidate) => candidate.job_id === job.job_id) : undefined);
    } catch (reason) {
      if (scope === scopeGeneration.current && current === refreshSequence.current && !controller.signal.aborted) onError(selfErrorMessage(reason, t, t('common.requestFailed')));
    } finally {
      if (scope === scopeGeneration.current && current === refreshSequence.current) setLoading(false);
      if (refreshController.current === controller) refreshInFlight.current = false;
    }
  }, [credential]);

  useEffect(() => {
    scopeGeneration.current += 1;
    refreshSequence.current += 1;
    refreshController.current?.abort();
    actionControllers.current.abortAll();
    refreshInFlight.current = false;
    setJobs([]);
    setSelected(undefined);
    setMessage('');
    setCancellingIds(new Set());
    setLoading(true);
    onError('');
    void refresh();
    return () => {
      scopeGeneration.current += 1;
      refreshSequence.current += 1;
      refreshController.current?.abort();
      actionControllers.current.abortAll();
      refreshInFlight.current = false;
    };
  }, [credential, refresh]);

  const pending = jobs.some((job) => job.status === 'queued' || job.status === 'running' || job.status === 'cancelling');
  useEffect(() => {
    if (!pending) return;
    return startCompletionPolling(refresh, 1_000);
  }, [pending, refresh]);

  async function cancel(job: GenerationJob) {
    const actionKey = `cancel:${job.job_id}`;
    if (actionControllers.current.has(actionKey) || cancellingIds.has(job.job_id) || !window.confirm(t('generations.confirmCancel', { model: job.model }))) return;
    const scope = scopeGeneration.current;
    const controller = actionControllers.current.begin(actionKey);
    if (!controller) return;
    onError('');
    setMessage('');
    setCancellingIds((ids) => new Set(ids).add(job.job_id));
    try {
      const cancelled = await api<GenerationJob>(`/self/v1/generations/${job.job_id}`, credential, { method: 'DELETE', signal: controller.signal });
      if (scope !== scopeGeneration.current || controller.signal.aborted) return;
      setJobs((current) => current.map((candidate) => candidate.job_id === cancelled.job_id ? cancelled : candidate));
      setSelected((current) => current?.job_id === cancelled.job_id ? cancelled : current);
      setMessage(t(cancelled.status === 'cancelling' ? 'self.generationCancellationRequested' : 'self.generationCancelled'));
    } catch (reason) {
      if (!controller.signal.aborted && scope === scopeGeneration.current) onError(reason instanceof ApiError && reason.status === 400
        ? t('self.generationCancelFailed')
        : selfErrorMessage(reason, t, t('self.generationCancelFailed')));
    } finally {
      actionControllers.current.finish(actionKey, controller);
      if (scope === scopeGeneration.current) setCancellingIds((ids) => {
        const next = new Set(ids);
        next.delete(job.job_id);
        return next;
      });
    }
  }

  async function download(job: GenerationJob, asset: GenerationAsset) {
    const scope = scopeGeneration.current;
    const actionKey = `download:${job.job_id}:${asset.asset_id}`;
    const controller = actionControllers.current.begin(actionKey);
    if (!controller) return;
    try {
      const response = await fetch(`/self/v1/generations/${job.job_id}/assets/${asset.asset_id}`, {
        headers: { Authorization: `Bearer ${credential}` },
        signal: controller.signal,
      });
      if (scope !== scopeGeneration.current || controller.signal.aborted) return;
      if (!response.ok) throw new ApiError(`HTTP ${response.status}`, response.status);
      const objectUrl = URL.createObjectURL(await response.blob());
      const link = document.createElement('a');
      link.href = objectUrl;
      link.download = asset.filename;
      link.click();
      URL.revokeObjectURL(objectUrl);
    } catch (reason) {
      if (!controller.signal.aborted && scope === scopeGeneration.current) onError(selfErrorMessage(reason, t, t('self.assetDownloadFailed')));
    } finally {
      actionControllers.current.finish(actionKey, controller);
    }
  }

  return <div className="self-page self-generations-page" data-self-page="generations">
    {message && <div className="notice success" role="status">{message}</div>}
    <article className="panel self-generations">
      <div className="panel-title"><h2>{t('self.generations')}</h2><button type="button" className="secondary" disabled={loading} onClick={() => void refresh()}>{loading ? t('common.loading') : t('self.refreshGenerations')}</button></div>
      {loading && jobs.length === 0 ? <div className="boot">{t('common.loading')}</div> : <div className="table-scroll"><table><thead><tr><th>{t('request.time')}</th><th>{t('request.model')}</th><th>{t('request.status')}</th><th>{t('self.units')}</th><th>{t('request.cost')}</th><th>{t('request.error')}</th><th>{t('routes.actions')}</th></tr></thead><tbody>{jobs.map((job) => <tr key={job.job_id}><td>{new Date(job.created_at).toLocaleString(locale)}</td><td><button type="button" className="table-link" onClick={() => setSelected(job)} aria-label={t('self.openGeneration', { model: job.model })}><code>{job.model}</code></button></td><td><span className={`status ${job.status === 'succeeded' ? 'ok' : job.status === 'failed' || job.status === 'cancelled' ? 'bad' : 'pending'}`}>{t(`generationStatus.${job.status}`)}</span></td><td>{job.billed_units === null ? `≤ ${formatNumber(job.estimated_units, locale)}` : formatNumber(job.billed_units, locale)}</td><td>{formatCurrency(job.cost, credentialView.currency, locale)}</td><td>{job.error_code ?? '—'}</td><td>{(job.status === 'queued' || job.status === 'running') && <button type="button" className="secondary" disabled={cancellingIds.has(job.job_id)} onClick={() => void cancel(job)}>{cancellingIds.has(job.job_id) ? t('common.loading') : t('self.cancelGeneration')}</button>}</td></tr>)}</tbody></table>{jobs.length === 0 && <div className="empty">{t('self.noGenerations')}</div>}</div>}
    </article>
    {selected && <GenerationDrawer job={selected} currency={credentialView.currency} cancelling={cancellingIds.has(selected.job_id)} onDownload={(asset) => void download(selected, asset)} onCancel={() => void cancel(selected)} onClose={() => setSelected(undefined)} />}
  </div>;
}
