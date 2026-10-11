import type { UpstreamQuotaBatchResponse, UpstreamQuotaSnapshot } from '../../src/operator/upstreamQuota.js';

export interface QuotaBatchFixtureAccount {
  id: string;
  tenant: string;
  driver?: string;
}

export function quotaBatchFixtureSnapshot(account: QuotaBatchFixtureAccount): UpstreamQuotaSnapshot {
  const now = Date.now();
  return {
    contract_version: 'upstream_quota_v1',
    upstream_account_id: account.id,
    tenant_external_id: account.tenant,
    provider: account.driver ?? 'fixture-driver',
    status: 'ready',
    observed_at: now,
    stale_after: now + 60_000,
    stale: false,
    freshness: 'fresh',
    plan_type: null,
    workspace: null,
    capabilities: {
      read: true, plan: true, workspace: false, window_amounts: false, window_amount_unit: false,
      window_percent: true, reset_credit_expiry: false, subscription_expiry: false,
      supplier_read_only: true, refreshes_credentials: false, consumes_reset_credit: false,
    },
    subscription_active_until: null,
    credits: { balance: null, unlimited: null, has_credits: null, source: null },
    windows: [{
      id: 'primary', label: 'Fixture quota window', used_percent: 25, used: null, remaining: null, limit: null,
      unit: null, reset_at: now + 3_600_000, period_seconds: null, source: 'provider_usage',
      reset_is_estimated: false, allowed: true, limit_reached: false,
    }],
    error_code: null,
    attempts: [],
    cache_hit: false,
    reset_credits: [],
    reset_capability: {
      provider_supported: false, implementation_available: false, prepare_available: false,
      confirmation_required: false, retryable: false, available_credits: null, applicable_credits: null,
      reason: 'quota_reset_not_supported', credit_error_code: null, evidence: 'server_driver_contract',
    },
  };
}

/**
 * Strict batch contract for the automatic list quota read: only a fresh bulk
 * read over exactly the fixture's eligible accounts is answered. Unknown
 * accounts or unexpected payloads throw instead of masking the request.
 */
export function quotaBatchFixtureResponse(body: unknown, accounts: QuotaBatchFixtureAccount[]): UpstreamQuotaBatchResponse {
  const request = body as { account_ids?: unknown; fresh?: unknown; trigger?: unknown } | null;
  if (!request || !Array.isArray(request.account_ids) || request.fresh !== true || request.trigger !== 'bulk') {
    throw new Error('Quota batch fixture expects a fresh bulk read');
  }
  const eligible = accounts.filter(account => request.account_ids!.includes(account.id));
  if (eligible.length !== request.account_ids.length || new Set(request.account_ids).size !== request.account_ids.length) {
    throw new Error(`Quota batch fixture received unknown account ids: ${JSON.stringify(request.account_ids)}`);
  }
  return {
    contract_version: 'upstream_quota_batch_v1',
    results: eligible.map(account => ({ status: 'success', upstream_account_id: account.id, snapshot: quotaBatchFixtureSnapshot(account) })),
  };
}
