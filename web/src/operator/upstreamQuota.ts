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
    prepare_available?: boolean;
    confirmation_required?: boolean;
    retryable?: boolean;
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
