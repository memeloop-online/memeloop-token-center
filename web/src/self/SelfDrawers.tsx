import { DrawerFrame } from '../components';
import { formatCurrency, formatMilliseconds, formatNumber } from '../format';
import { useI18n } from '../i18n';
import type { GenerationAsset, GenerationJob, RequestDetail } from '../types';

export function RequestDetailDrawer({ detail, currency, onClose }: { detail: RequestDetail; currency?: string; onClose: () => void }) {
  const { locale, t } = useI18n();
  const successful = detail.status_code !== null && detail.status_code < 400;
  return <DrawerFrame title={detail.model} eyebrow={t('request.detail')} onClose={onClose}>
    <p className="muted break-anywhere request-identity">{detail.request_id}</p>
    <div className="request-diagnostics">
      <span><b>{t('request.time')}</b>{new Date(detail.created_at).toLocaleString(locale)}</span>
      <span><b>{t('request.status')}</b><i className={`status ${successful ? 'ok' : detail.status_code ? 'bad' : 'pending'}`}>{detail.status_code ?? t('common.running')}</i></span>
      <span><b>{t('request.protocol')}</b>{detail.protocol}</span>
      <span><b>{t('request.duration')}</b>{formatMilliseconds(detail.duration_ms, locale)}</span>
      <span><b>{t('request.tokens')}</b>{formatNumber(detail.input_tokens + detail.output_tokens, locale)} <small>{formatNumber(detail.input_tokens, locale)} + {formatNumber(detail.output_tokens, locale)}</small></span>
      <span><b>{t('request.cost')}</b>{currency ? formatCurrency(detail.cost, currency, locale) : '—'}</span>
      <span><b>{t('request.error')}</b>{detail.error_code ?? '—'}</span>
      <span><b>{t('self.archive')}</b>{detail.archive_complete ? t('request.archiveComplete') : t('request.archiveIncomplete')}</span>
      {detail.provenance && <span><b>{t('request.provenance')}</b>{detail.provenance.unlinked ? t('request.archiveOnly') : t('request.exactArchive')} · {detail.provenance.source}</span>}
    </div>
    <h3>{t('request.request')}</h3><pre>{JSON.stringify(detail.request_body, null, 2)}</pre>
    <h3>{t('request.response')}</h3><pre>{JSON.stringify(detail.response_body, null, 2)}</pre>
  </DrawerFrame>;
}

export function GenerationDrawer({ job, currency, cancelling = false, onDownload, onCancel, onClose }: {
  job: GenerationJob;
  currency?: string;
  cancelling?: boolean;
  onDownload: (asset: GenerationAsset) => void;
  onCancel: () => void;
  onClose: () => void;
}) {
  const { locale, t } = useI18n();
  return <DrawerFrame title={job.model} eyebrow={t('self.generationDetail')} onClose={onClose}>
    <p className="muted break-anywhere">{job.job_id} · {t(`generationStatus.${job.status}`)}</p>
    {(job.status === 'queued' || job.status === 'running') && <button type="button" className="danger" disabled={cancelling} onClick={onCancel}>{cancelling ? t('common.loading') : t('self.cancelGeneration')}</button>}
    <h3>{t('self.billing')}</h3>
    <div className="request-diagnostics"><span><b>{t('self.units')}</b>{job.billed_units === null ? `≤ ${formatNumber(job.estimated_units, locale)}` : formatNumber(job.billed_units, locale)}</span><span><b>{t('request.cost')}</b>{currency ? formatCurrency(job.cost, currency, locale) : '—'}</span></div>
    <h3>{t('self.resultArchive')}</h3>
    {job.assets.length > 0 ? <div className="account-list">{job.assets.map((asset) => <div className="account" key={asset.asset_id}>
      <div className="account-main"><b>{asset.filename}</b><span>{asset.mime_type} · {formatNumber(asset.size_bytes, locale)} {t('self.bytes')}</span></div>
      <button type="button" className="secondary" onClick={() => onDownload(asset)}>{t('self.downloadAsset')}</button>
    </div>)}</div> : <div className="empty">{t('self.noAssets')}</div>}
    {job.error_code && <><h3>{t('request.error')}</h3><pre>{job.error_code}</pre></>}
  </DrawerFrame>;
}
