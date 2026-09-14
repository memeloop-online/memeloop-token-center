export interface UpstreamQuotaSnapshot {
  contract_version: 'upstream_quota_v1';
  upstream_account_id: string;
  tenant_external_id: string;
  provider: string;
  status: 'ready' | 'error' | 'unsupported';
  observed_at: number | null;
  stale_after: number | null;
  stale: boolean;
  plan_type: string | null;
  credits: { balance: string | null; unlimited: boolean | null; has_credits: boolean | null };
  windows: {
    id: string;
    label: string;
    used_percent: number | null;
    remaining: number | null;
    limit: number | null;
    reset_at: number | null;
    period_seconds: number | null;
    source: string;
    reset_is_estimated: boolean;
    allowed: boolean | null;
    limit_reached: boolean | null;
  }[];
  reset_capability: {
    provider_supported: boolean | null;
    implementation_available: boolean;
    prepare_available: boolean;
    confirmation_required: boolean;
    retryable: boolean;
    available_credits: number | null;
    applicable_credits: number | null;
    reason: string | null;
    credit_error_code: string | null;
  };
  error_code: string | null;
}

export function upstreamQuotaPath(accountId: string, tenant: string) {
  if (!accountId || !tenant.trim()) throw new Error('Quota requires an account and tenant');
  return `/internal/v1/upstreams/${encodeURIComponent(accountId)}/quota?${new URLSearchParams({ tenant_external_id: tenant })}`;
}

export function quotaUsedPercent(window: UpstreamQuotaSnapshot['windows'][number]): number | null {
  if (window.used_percent !== null && Number.isFinite(window.used_percent)) return window.used_percent;
  if (window.remaining !== null && window.limit !== null && window.limit > 0) return (1 - window.remaining / window.limit) * 100;
  return null;
}

export type QuotaObservationState = 'unobserved' | 'current' | 'historical';

/** A retained snapshot is evidence from its observation time, never a current successful read. */
export function quotaObservationState(snapshot: UpstreamQuotaSnapshot, now = Date.now(), refreshFailed = false): QuotaObservationState {
  if (snapshot.observed_at === null) return 'unobserved';
  if (refreshFailed || snapshot.status === 'error' || snapshot.error_code || snapshot.stale
    || (snapshot.stale_after !== null && now >= snapshot.stale_after)) return 'historical';
  return 'current';
}

export interface QuotaSummaryPresentation {
  key: string;
  usedPercent: number | null;
}

export function quotaSummaryPresentation(snapshot: UpstreamQuotaSnapshot | undefined, now = Date.now(), refreshFailed = false): QuotaSummaryPresentation {
  if (!snapshot) return { key: refreshFailed ? 'providerDirectory.readFailed' : 'providerDirectory.notChecked', usedPercent: null };
  if (snapshot.status === 'unsupported') return { key: 'providerDirectory.unsupported', usedPercent: null };
  const observation = quotaObservationState(snapshot, now, refreshFailed);
  const readFailed = refreshFailed || snapshot.status === 'error' || Boolean(snapshot.error_code);
  if (observation === 'unobserved') return { key: readFailed ? 'providerDirectory.readFailed' : 'providerDirectory.usageUnavailable', usedPercent: null };
  const percents = snapshot.windows.map(quotaUsedPercent).filter((percent): percent is number => percent !== null);
  const usedPercent = percents.length ? Math.max(...percents) : null;
  if (readFailed) return { key: usedPercent === null ? 'providerDirectory.refreshFailedRetained' : 'providerDirectory.refreshFailedUsed', usedPercent };
  if (observation === 'historical') return { key: usedPercent === null ? 'providerDirectory.lastObservedUnavailable' : 'providerDirectory.lastObservedUsed', usedPercent };
  return { key: usedPercent === null ? 'providerDirectory.usageUnavailable' : 'providerDirectory.used', usedPercent };
}

export interface QuotaWindowPresentation {
  scopeKey: string;
  periodKey: string;
  supplierLabel: string | null;
  qualifier: string | null;
}

const PERIODS: [seconds: number, key: string][] = [
  [18_000, 'quota.periodFiveHour'],
  [86_400, 'quota.periodDaily'],
  [604_800, 'quota.periodWeekly'],
  [2_592_000, 'quota.periodMonthly'],
  [31_536_000, 'quota.periodAnnual'],
];

function quotaPeriodKey(periodSeconds: number | null, role: string | undefined) {
  if (periodSeconds !== null && Number.isFinite(periodSeconds) && periodSeconds > 0) {
    const period = PERIODS.find(([seconds]) => Math.abs(periodSeconds - seconds) <= seconds * 0.05);
    if (period) return period[1];
  }
  if (role === 'primary_window') return 'quota.periodPrimary';
  if (role === 'secondary_window') return 'quota.periodSecondary';
  return 'quota.periodSupplier';
}

/**
 * Codex names durations from the supplier's window length (matching the official
 * Codex status surface); primary/secondary are only fallbacks when no duration exists.
 */
export function quotaWindowPresentation(provider: string, window: UpstreamQuotaSnapshot['windows'][number]): QuotaWindowPresentation {
  const match = /^([^:]+):(primary_window|secondary_window)$/.exec(window.id);
  const role = match?.[2];
  const periodKey = quotaPeriodKey(window.period_seconds, role);
  if (provider === 'openai-codex') {
    const scopeKey = match?.[1] === 'code' ? 'quota.scopeCodex'
      : match?.[1] === 'code_review' ? 'quota.scopeCodexReview'
      : 'quota.scopeCodexAdditional';
    const qualifier = scopeKey !== 'quota.scopeCodexAdditional' ? null
      : match?.[1] ? match[1].replace(/[_.-]+/g, ' ').replace(/\s+/g, ' ').trim() || null
      : window.label.trim() && window.label !== window.id ? window.label : null;
    return { scopeKey, periodKey, supplierLabel: null, qualifier };
  }
  const supplierLabel = window.label.trim() && window.label !== window.id ? window.label : null;
  return { scopeKey: provider === 'kimi-oauth' ? 'quota.scopeKimi' : 'quota.scopeSupplier', periodKey, supplierLabel, qualifier: null };
}

export function quotaSourceLabel(source: string) {
  if (source === 'codex_usage') return 'quota.sourceCodexUsage';
  if (source === 'kimi_usage') return 'quota.sourceKimiUsage';
  return 'quota.sourceSupplierResponse';
}

/** Only translate known normalized codes; never render supplier/error payloads. */
export function quotaReadErrorMessage(code: string | null | undefined) {
  switch (code) {
    case 'credential_invalid': return 'quota.errorCredential';
    case 'quota_not_authorized': return 'quota.errorSupplierAuthorization';
    case 'quota_destination_invalid': return 'quota.errorDestination';
    case 'quota_transport_failed': return 'quota.errorTransport';
    case 'quota_timeout': return 'quota.errorTimeout';
    case 'quota_rate_limited': return 'quota.errorRateLimited';
    case 'quota_busy':
    case 'quota_refresh_in_progress': return 'quota.errorBusy';
    case 'quota_response_too_large':
    case 'quota_too_many_windows':
    case 'quota_duplicate_window':
    case 'quota_invalid_credit_payload':
    case 'quota_too_many_credits':
    case 'quota_incomplete_credit_payload':
    case 'quota_invalid_payload': return 'quota.errorPayload';
    case 'quota_upstream_error': return 'quota.errorSupplier';
    default: return 'quota.readFailed';
  }
}
