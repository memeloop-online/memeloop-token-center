import { DetailTooltip, ProgressBar } from '../design-system';
import { formatNumber, formatPercent } from '../format';
import { useI18n } from '../i18n';
import { quotaAvailableResetCredits, quotaHighestUsageWindow, quotaObservationState, quotaRemaining, quotaResetCreditExpiry, quotaSummaryPresentation, quotaUnitMessage, quotaUsedPercent, quotaWindowPresentation, type UpstreamQuotaSnapshot } from './upstreamQuota';
import './upstreamQuota.css';
import { useQuotaClock } from './useQuotaClock';

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

export function QuotaSummary({
  snapshot,
  refreshFailed = false,
  now: suppliedNow,
  mode = 'used',
  showWindowReset = false,
  showResetCreditExpiryInTooltip = false,
}: {
  snapshot?: UpstreamQuotaSnapshot;
  refreshFailed?: boolean;
  now?: number;
  mode?: 'used' | 'remaining';
  showWindowReset?: boolean;
  showResetCreditExpiryInTooltip?: boolean;
}) {
  const { t, locale } = useI18n();
  const clock = useQuotaClock();
  const now = suppliedNow ?? clock;
  const label = useQuotaWindowLabel();
  const presentation = quotaSummaryPresentation(snapshot, now, refreshFailed);
  const highest = presentation.usedPercent !== null && snapshot ? quotaHighestUsageWindow(snapshot.windows) : undefined;
  const resetCreditExpiry = snapshot ? quotaResetCreditExpiry(snapshot, now) : undefined;
  const showResetCreditExpiry = Boolean(
    snapshot
    && showResetCreditExpiryInTooltip
    && resetCreditExpiry?.state === 'known'
    && (quotaAvailableResetCredits(snapshot, now) ?? 0) > 1,
  );
  const text = t(presentation.key, {
    name: highest && snapshot ? label(snapshot.provider, highest) : '',
    percent: formatPercent(presentation.usedPercent === null ? null : presentation.usedPercent / 100, locale),
  });
  if (!snapshot || quotaObservationState(snapshot, now, refreshFailed) === 'unobserved' || snapshot.status === 'unsupported' || !snapshot.windows.length) return <span className="quota-summary-status">{text}</span>;
  const historical = quotaObservationState(snapshot, now, refreshFailed) === 'historical';
  const resetText = (window: UpstreamQuotaSnapshot['windows'][number]) => {
    if (window.reset_at === null || !Number.isFinite(window.reset_at)) return undefined;
    if (window.reset_at <= now) return t('quota.resetElapsed');
    return t(window.reset_is_estimated ? 'quota.estimatedResetAt' : 'quota.resetAt', { time: new Date(window.reset_at).toLocaleString(locale) });
  };
  const remainingText = (window: UpstreamQuotaSnapshot['windows'][number]) => {
    const remaining = quotaRemaining(window);
    if (!remaining) return t('quota.remainingUnknown');
    if (remaining.kind === 'percent') return t('quota.remainingPercent', { percent: formatPercent(remaining.percent / 100, locale) });
    const unitKey = quotaUnitMessage(remaining.unit);
    return t('quota.remainingWithUnit', { amount: formatNumber(remaining.amount, locale), limit: remaining.limit === null ? '—' : formatNumber(remaining.limit, locale), unit: unitKey ? t(unitKey) : remaining.unit });
  };
  const content = <div className="quota-summary-tooltip">
    <p>{t(historical ? 'quota.lastObservedAt' : 'quota.observedAt', { time: new Date(snapshot.observed_at!).toLocaleString(locale) })}</p>
    {historical && <p>{text}</p>}
    {showWindowReset && highest && resetText(highest) && <p>{resetText(highest)}</p>}
    {showResetCreditExpiry && <p>{t('quota.creditExpiresAt', { time: new Date(resetCreditExpiry!.at!).toLocaleString(locale) })}</p>}
    <ul>{snapshot.windows.map(window => <li key={window.id}>
      <span>{label(snapshot.provider, window)}</span>
      <strong>{mode === 'remaining' ? t('quota.usedPercentValue', { percent: formatPercent(quotaUsedPercent(window) === null ? null : quotaUsedPercent(window)! / 100, locale) }) : formatPercent(quotaUsedPercent(window) === null ? null : quotaUsedPercent(window)! / 100, locale)}</strong>
      {mode === 'remaining' && <span>{remainingText(window)}</span>}
      {window.limit_reached === true && <span>{t('quota.limitReached')}</span>}
      {window.allowed === false && window.limit_reached !== true && <span>{t('quota.notAllowed')}</span>}
    </li>)}</ul>
  </div>;
  return <DetailTooltip content={content}><span className="quota-summary" tabIndex={0}>
    {mode === 'remaining' && highest ? <>
      <span>{label(snapshot.provider, highest)}</span>
      <span className="quota-summary-value">{remainingText(highest)}</span>
      {historical && <span className="quota-summary-provenance">{text}</span>}
    </> : <>
      <span>{text}</span>
      {showWindowReset && highest && resetText(highest) && <span className="quota-summary-reset">{resetText(highest)}</span>}
    </>}
    {highest && (mode === 'used' || quotaRemaining(highest) !== null) && <ProgressBar className="quota-summary-meter" max={100} value={Math.max(0, Math.min(100, mode === 'remaining' ? 100 - presentation.usedPercent! : presentation.usedPercent!))} role="meter" aria-valuetext={mode === 'remaining' ? `${label(snapshot.provider, highest)} ${remainingText(highest)}${historical ? ` · ${text}` : ''}` : text} aria-label={t(mode === 'remaining' ? 'quota.remainingWindow' : historical ? 'quota.lastObservedUsedPercent' : 'quota.usedPercent', { name: label(snapshot.provider, highest) })} />}
  </span></DetailTooltip>;
}
