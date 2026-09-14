import assert from 'node:assert/strict';
import test from 'node:test';
import { quotaObservationState, quotaSummaryPresentation, quotaUsedPercent, quotaWindowPresentation, upstreamQuotaPath, type UpstreamQuotaSnapshot } from '../src/operator/upstreamQuota.js';

test('quota URL requires and preserves explicit account and tenant identity', () => {
  const url = new URL(upstreamQuotaPath('account/one', 'tenant & one'), 'https://example.test');
  assert.equal(url.pathname, '/internal/v1/upstreams/account%2Fone/quota');
  assert.equal(url.searchParams.get('tenant_external_id'), 'tenant & one');
  assert.throws(() => upstreamQuotaPath('account', ''));
});

test('unknown windows stay unknown and exact usage is not rounded or capped by the projection', () => {
  const window: UpstreamQuotaSnapshot['windows'][number] = { id: 'primary', label: 'Primary', used_percent: null, remaining: null, limit: null, reset_at: null, period_seconds: null, source: 'provider', reset_is_estimated: false, allowed: null, limit_reached: null };
  assert.equal(quotaUsedPercent(window), null);
  assert.equal(quotaUsedPercent({ ...window, remaining: 25, limit: 100 }), 75);
  assert.equal(quotaUsedPercent({ ...window, remaining: 0, limit: 0 }), null);
  assert.equal(quotaUsedPercent({ ...window, used_percent: 120.25 }), 120.25);
});

test('Codex window cadence follows supplier duration before internal primary or secondary role', () => {
  const window: UpstreamQuotaSnapshot['windows'][number] = { id: 'code:primary_window', label: 'code:primary_window', used_percent: 0, remaining: null, limit: null, reset_at: null, period_seconds: 18_000, source: 'codex_usage', reset_is_estimated: false, allowed: true, limit_reached: false };
  assert.deepEqual(quotaWindowPresentation('openai-codex', window), {
    scopeKey: 'quota.scopeCodex', periodKey: 'quota.periodFiveHour', supplierLabel: null, qualifier: null,
  });
  assert.equal(quotaWindowPresentation('openai-codex', { ...window, id: 'code:secondary_window', period_seconds: 604_800 }).periodKey, 'quota.periodWeekly');
  assert.equal(quotaWindowPresentation('openai-codex', { ...window, period_seconds: 604_800 }).periodKey, 'quota.periodWeekly');
  assert.equal(quotaWindowPresentation('openai-codex', { ...window, period_seconds: null }).periodKey, 'quota.periodPrimary');
  assert.equal(quotaWindowPresentation('openai-codex', { ...window, id: 'code_review:primary_window' }).scopeKey, 'quota.scopeCodexReview');
  assert.equal(quotaWindowPresentation('openai-codex', { ...window, id: 'bengalfox_tokens:primary_window' }).qualifier, 'bengalfox tokens');
  assert.equal(quotaWindowPresentation('openai-codex', { ...window, id: 'legacy-primary', label: 'Supplier feature' }).qualifier, 'Supplier feature');
});

test('quota observation state does not confuse a failed refresh or expired snapshot with current data', () => {
  const snapshot: UpstreamQuotaSnapshot = {
    contract_version: 'upstream_quota_v1', upstream_account_id: 'account', tenant_external_id: 'tenant', provider: 'openai-codex',
    status: 'ready', observed_at: 1_000, stale_after: 2_000, stale: false, plan_type: null,
    credits: { balance: null, unlimited: null, has_credits: null }, windows: [],
    reset_capability: { provider_supported: true, implementation_available: false, prepare_available: false, confirmation_required: false, retryable: false, available_credits: null, applicable_credits: null, reason: null, credit_error_code: null },
    error_code: null,
  };
  assert.equal(quotaObservationState(snapshot, 1_500), 'current');
  assert.equal(quotaObservationState(snapshot, 1_500, true), 'historical');
  assert.equal(quotaObservationState({ ...snapshot, error_code: 'quota_transport_failed' }, 1_500), 'historical');
  assert.equal(quotaObservationState(snapshot, 2_000), 'historical');
  assert.equal(quotaObservationState({ ...snapshot, observed_at: null }, 1_500), 'unobserved');
  const zeroWindow = { id: 'code:primary_window', label: 'code:primary_window', used_percent: 0, remaining: null, limit: null, reset_at: null, period_seconds: 18_000, source: 'codex_usage', reset_is_estimated: false, allowed: true, limit_reached: false };
  assert.deepEqual(quotaSummaryPresentation({ ...snapshot, windows: [zeroWindow], error_code: 'quota_transport_failed' }, 1_500), { key: 'providerDirectory.refreshFailedUsed', usedPercent: 0 });
  assert.deepEqual(quotaSummaryPresentation({ ...snapshot, status: 'error', observed_at: null, windows: [zeroWindow] }, 1_500), { key: 'providerDirectory.readFailed', usedPercent: null });
});
