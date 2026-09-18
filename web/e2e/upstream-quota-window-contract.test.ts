import assert from 'node:assert/strict';
import test from 'node:test';
import { formatCountdown } from '../src/format.js';
import { UPSTREAM_QUOTA_READ_TIMEOUT_MILLIS, quotaHighestUsageWindow, quotaObservationState, quotaRemaining, quotaResetCreditExpiry, quotaSummaryPresentation, quotaUnitMessage, quotaUsedPercent, quotaWindowPresentation, upstreamQuotaBatchPath, upstreamQuotaPath, type UpstreamQuotaSnapshot } from '../src/operator/upstreamQuota.js';

test('quota URL requires and preserves explicit account and tenant identity', () => {
  assert.equal(UPSTREAM_QUOTA_READ_TIMEOUT_MILLIS, 85_000, 'operator deadline preserves ten seconds beyond the server read budget');
  const url = new URL(upstreamQuotaPath('account/one', 'tenant & one'), 'https://example.test');
  assert.equal(url.pathname, '/internal/v1/upstreams/account%2Fone/quota');
  assert.equal(url.searchParams.get('tenant_external_id'), 'tenant & one');
  assert.equal(url.searchParams.has('fresh'), false);
  assert.equal(url.searchParams.has('trigger'), false);
  const manual = new URL(upstreamQuotaPath('account/one', 'tenant & one', { fresh: true, trigger: 'manual' }), 'https://example.test');
  assert.equal(manual.searchParams.get('fresh'), 'true');
  assert.equal(manual.searchParams.get('trigger'), 'manual');
  const bulk = new URL(upstreamQuotaPath('account/one', 'tenant & one', { fresh: true, trigger: 'bulk' }), 'https://example.test');
  assert.equal(bulk.searchParams.get('fresh'), 'true');
  assert.equal(bulk.searchParams.get('trigger'), 'bulk');
  assert.throws(() => upstreamQuotaPath('account', ''));
});

test('quota batch URL carries no caller-selected tenant', () => {
  const url = new URL(upstreamQuotaBatchPath(), 'https://example.test');
  assert.equal(url.pathname, '/internal/v1/upstreams/quota/batch');
  assert.equal(url.search, '');
});

test('unknown windows stay unknown and exact usage is not rounded or capped by the projection', () => {
  const window: UpstreamQuotaSnapshot['windows'][number] = { id: 'primary', label: 'Primary', used_percent: null, used: null, remaining: null, limit: null, unit: null, reset_at: null, period_seconds: null, source: 'provider', reset_is_estimated: false, allowed: null, limit_reached: null };
  assert.equal(quotaUsedPercent(window), null);
  assert.equal(quotaUsedPercent({ ...window, remaining: 25, limit: 100 }), 75);
  assert.equal(quotaUsedPercent({ ...window, remaining: 0, limit: 0 }), null);
  assert.equal(quotaUsedPercent({ ...window, used_percent: 120.25 }), 120.25);
  assert.deepEqual(quotaRemaining({ ...window, unit: null, used_percent: 0, remaining: 100, limit: 100 }), { kind: 'percent', percent: 100 });
  assert.deepEqual(quotaRemaining({ ...window, used_percent: 25.5, remaining: 100, limit: 100 }), { kind: 'percent', percent: 74.5 });
  assert.equal(quotaRemaining({ ...window, remaining: 100, limit: 100 }), null, 'unknown unit and unknown used percentage cannot imply 100 absolute credits');
  assert.equal(quotaRemaining({ ...window, used_percent: Number.NaN }), null);
  assert.equal(quotaRemaining({ ...window, used_percent: 120.25 }), null, 'out-of-range usage is not clamped into invented remaining quota');
  assert.deepEqual(quotaRemaining({ ...window, unit: 'requests', remaining: 25, limit: 100 }), { kind: 'amount', amount: 25, limit: 100, unit: 'requests' });
  assert.equal(quotaUnitMessage('requests'), 'quota.unitRequests');
  assert.equal(quotaUnitMessage('Tokens'), 'quota.unitTokens');
  assert.equal(quotaUnitMessage('Vendor Compute Units'), null, 'unrecognized supplier units stay verbatim');
});

test('quota countdown keeps minute, hour and day boundaries without sleeping or changing exact deadlines', () => {
  const now = Date.UTC(2026, 8, 15);
  for (const [milliseconds, zh, en] of [
    [0, '0分钟', '0m'], [59_999, '不到1分钟', 'less than 1 min'], [60_000, '1分钟', '1m'],
    [3_599_999, '59分钟', '59m'], [3_600_000, '1小时', '1h'],
    [86_399_999, '23小时59分钟', '23h 59m'], [86_400_000, '1天', '1d'],
    [167 * 3_600_000, '6天23小时', '6d 23h'], [90_180_000, '1天1小时3分钟', '1d 1h 3m'],
  ] as const) {
    const resetAt = now + milliseconds;
    assert.equal(formatCountdown(resetAt - now, 'zh-CN'), zh);
    assert.equal(formatCountdown(resetAt - now, 'en'), en);
  }
  for (const value of [null, undefined, Number.NaN, Infinity, -1]) assert.equal(formatCountdown(value, 'zh-CN'), '—');
});

test('compact quota selects a real named window, preserves ties and ignores unknown usage', () => {
  const window: UpstreamQuotaSnapshot['windows'][number] = { id: 'code:primary_window', label: 'code:primary_window', used_percent: 51, used: null, unit: null, remaining: null, limit: null, reset_at: null, period_seconds: 18_000, source: 'codex_usage', reset_is_estimated: false, allowed: true, limit_reached: false };
  const weekly = { ...window, id: 'code:secondary_window', used_percent: 20, period_seconds: 604_800 };
  assert.equal(quotaHighestUsageWindow([weekly, window]), window);
  assert.equal(quotaHighestUsageWindow([window, { ...weekly, used_percent: 51 }]), window);
  assert.equal(quotaHighestUsageWindow([{ ...window, used_percent: null }]), undefined);
  assert.equal(quotaHighestUsageWindow([]), undefined);
  assert.equal(quotaUsedPercent({ ...window, used_percent: null, remaining: Infinity, limit: 100 }), null);
  assert.equal(quotaUsedPercent({ ...window, used_percent: null, remaining: 1, limit: Infinity }), null);
  const exceeded = { ...weekly, used_percent: 120.25 };
  assert.equal(quotaHighestUsageWindow([window, exceeded]), exceeded, 'only meter rendering clamps, never source evidence');
});

test('Codex window cadence follows supplier duration before internal primary or secondary role', () => {
  const window: UpstreamQuotaSnapshot['windows'][number] = { id: 'code:primary_window', label: 'code:primary_window', used_percent: 0, used: null, remaining: null, limit: null, unit: null, reset_at: null, period_seconds: 18_000, source: 'codex_usage', reset_is_estimated: false, allowed: true, limit_reached: false };
  assert.deepEqual(quotaWindowPresentation('openai-codex', window), {
    scopeKey: 'quota.scopeCodex', periodKey: 'quota.periodFiveHour', supplierLabel: null, qualifier: null,
  });
  assert.equal(quotaWindowPresentation('openai-codex', { ...window, id: 'code:secondary_window', period_seconds: 604_800 }).periodKey, 'quota.periodWeekly');
  assert.equal(quotaWindowPresentation('openai-codex', { ...window, period_seconds: 604_800 }).periodKey, 'quota.periodWeekly');
  assert.equal(quotaWindowPresentation('openai-codex', { ...window, period_seconds: null }).periodKey, 'quota.periodPrimary');
  assert.equal(quotaWindowPresentation('openai-codex', { ...window, id: 'code_review:primary_window' }).scopeKey, 'quota.scopeCodexReview');
  assert.equal(quotaWindowPresentation('openai-codex', { ...window, id: 'bengalfox_tokens:primary_window' }).qualifier, null, 'internal feature names are not model identities');
  assert.equal(quotaWindowPresentation('openai-codex', { ...window, id: 'legacy-primary', label: 'Supplier feature' }).qualifier, 'Supplier feature');
});

test('Kimi weekly usage is named semantically while unknown limit cadence is not guessed from reset dates', () => {
  const window: UpstreamQuotaSnapshot['windows'][number] = { id: 'limit-0', label: 'limit-0', used_percent: 0, used: null, remaining: 100, limit: null, unit: null, reset_at: 1789428406870, period_seconds: null, source: 'kimi_usage', reset_is_estimated: false, allowed: null, limit_reached: null };
  assert.deepEqual(quotaWindowPresentation('kimi-oauth', window), {
    scopeKey: 'quota.scopeKimi', periodKey: 'quota.periodSupplier', supplierLabel: null, qualifier: null,
  });
  assert.equal(quotaWindowPresentation('kimi-oauth', { ...window, period_seconds: 18_000 }).periodKey, 'quota.periodFiveHour');
  for (const reset_at of [1789655206870, 1789465086257]) {
    const summary = { ...window, id: 'summary', label: 'summary', reset_at };
    assert.equal(quotaWindowPresentation('kimi-oauth', summary).periodKey, 'quota.periodWeekly');
    assert.equal(summary.reset_at, reset_at, 'presentation preserves the exact supplier reset date');
  }
  assert.equal(quotaWindowPresentation('other-provider', { ...window, id: 'summary', label: 'summary' }).periodKey, 'quota.periodSupplier');
});

test('quota observation state does not confuse a failed refresh or expired snapshot with current data', () => {
  const snapshot: UpstreamQuotaSnapshot = {
    contract_version: 'upstream_quota_v1', upstream_account_id: 'account', tenant_external_id: 'tenant', provider: 'openai-codex',
    status: 'ready', observed_at: 1_000, stale_after: 2_000, stale: false, freshness: 'fresh', plan_type: null, workspace: null,
    capabilities: { read: true, plan: true, workspace: false, window_amounts: false, window_amount_unit: false, window_percent: true, reset_credit_expiry: true, subscription_expiry: false, supplier_read_only: true, refreshes_credentials: false, consumes_reset_credit: false },
    subscription_active_until: null, credits: { balance: null, unlimited: null, has_credits: null, source: null }, windows: [], reset_credits: [],
    reset_capability: { provider_supported: true, implementation_available: false, prepare_available: false, confirmation_required: false, retryable: false, available_credits: null, applicable_credits: null, reason: 'quota_reset_not_supported', credit_error_code: null, evidence: 'server_driver_contract' },
    error_code: null,
  };
  assert.equal(quotaObservationState(snapshot, 1_500), 'current');
  assert.equal(quotaObservationState(snapshot, 1_500, true), 'historical');
  assert.equal(quotaObservationState({ ...snapshot, error_code: 'quota_transport_failed' }, 1_500), 'historical');
  assert.equal(quotaObservationState(snapshot, 2_000), 'historical');
  assert.equal(quotaObservationState({ ...snapshot, observed_at: null }, 1_500), 'unobserved');
  const zeroWindow = { id: 'code:primary_window', label: 'code:primary_window', used_percent: 0, used: null, remaining: null, limit: null, unit: null, reset_at: null, period_seconds: 18_000, source: 'codex_usage', reset_is_estimated: false, allowed: true, limit_reached: false };
  assert.deepEqual(quotaSummaryPresentation({ ...snapshot, windows: [zeroWindow], error_code: 'quota_transport_failed' }, 1_500), { key: 'providerDirectory.refreshFailedUsed', usedPercent: 0 });
  assert.deepEqual(quotaSummaryPresentation({ ...snapshot, status: 'error', observed_at: null, windows: [zeroWindow] }, 1_500), { key: 'providerDirectory.readFailed', usedPercent: null });
  const credit: UpstreamQuotaSnapshot['reset_credits'][number] = { status: 'available', granted_at: 1000, expires_at: 9000, source: 'codex_reset_credits' };
  assert.deepEqual(quotaResetCreditExpiry({ ...snapshot, reset_credits: [credit, { ...credit, expires_at: 5000 }, { ...credit, status: 'used', expires_at: 2000 }] }, 1500), { state: 'known', at: 5000 });
  assert.deepEqual(quotaResetCreditExpiry({ ...snapshot, reset_credits: [credit, { ...credit, expires_at: null }] }, 1500), { state: 'unknown' }, 'incomplete evidence cannot establish the earliest expiry');
  assert.deepEqual(quotaResetCreditExpiry(snapshot, 1500), { state: 'unknown' }, 'window reset dates cannot substitute for absent credit expiry');
  assert.deepEqual(quotaResetCreditExpiry({ ...snapshot, reset_credits: [credit] }, 10000), { state: 'none' }, 'expired credits never count as upcoming opportunities');
});
