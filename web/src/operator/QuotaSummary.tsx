import { DetailTooltip } from '../design-system';
import { formatPercent } from '../format';
import { useI18n } from '../i18n';
import { quotaHighestUsageWindow, quotaObservationState, quotaSummaryPresentation, quotaUsedPercent, quotaWindowPresentation, type UpstreamQuotaSnapshot } from './upstreamQuota';
import './upstreamQuota.css';

/** Compact and expanded quota surfaces share the supplier-window naming contract. */
export function useQuotaWindowLabel() {
  const { t } = useI18n();
  return (provider: string, window: UpstreamQuotaSnapshot['windows'][number]) => {
    const presentation = quotaWindowPresentation(provider, window);
    const baseScope = presentation.supplierLabel ?? t(presentation.scopeKey);
    const scope = presentation.qualifier ? t('quota.scopeWithQualifier', { scope: baseScope, qualifier: presentation.qualifier }) : baseScope;
    return t('quota.windowLabel', { scope, period: t(presentation.periodKey) });
  };
}

export function QuotaSummary({ snapshot, refreshFailed = false, now = Date.now() }: {
  snapshot?: UpstreamQuotaSnapshot; refreshFailed?: boolean; now?: number;
}) {
  const { t, locale } = useI18n();
  const label = useQuotaWindowLabel();
  const presentation = quotaSummaryPresentation(snapshot, now, refreshFailed);
  const highest = presentation.usedPercent !== null && snapshot ? quotaHighestUsageWindow(snapshot.windows) : undefined;
  const text = t(presentation.key, {
    name: highest && snapshot ? label(snapshot.provider, highest) : '',
    percent: formatPercent(presentation.usedPercent === null ? null : presentation.usedPercent / 100, locale),
  });
  if (!snapshot || quotaObservationState(snapshot, now, refreshFailed) === 'unobserved' || snapshot.status === 'unsupported' || !snapshot.windows.length) return <span>{text}</span>;
  const historical = quotaObservationState(snapshot, now, refreshFailed) === 'historical';
  const content = <div className="quota-summary-tooltip">
    <p>{t(historical ? 'quota.lastObservedAt' : 'quota.observedAt', { time: new Date(snapshot.observed_at!).toLocaleString(locale) })}</p>
    {historical && <p>{text}</p>}
    <ul>{snapshot.windows.map(window => <li key={window.id}>
      <span>{label(snapshot.provider, window)}</span>
      <strong>{formatPercent(quotaUsedPercent(window) === null ? null : quotaUsedPercent(window)! / 100, locale)}</strong>
      {window.limit_reached === true && <span>{t('quota.limitReached')}</span>}
      {window.allowed === false && window.limit_reached !== true && <span>{t('quota.notAllowed')}</span>}
    </li>)}</ul>
  </div>;
  return <DetailTooltip content={content}><span className="quota-summary" tabIndex={0}>
    <span>{text}</span>
    {highest && <meter className="quota-summary-meter" min={0} max={100} value={Math.max(0, Math.min(100, presentation.usedPercent!))} aria-label={t(historical ? 'quota.lastObservedUsedPercent' : 'quota.usedPercent', { name: label(snapshot.provider, highest) })} />}
  </span></DetailTooltip>;
}
