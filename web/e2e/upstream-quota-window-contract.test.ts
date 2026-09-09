import assert from 'node:assert/strict';
import test from 'node:test';
import { quotaUsedPercent, upstreamQuotaPath, type UpstreamQuotaSnapshot } from '../src/operator/upstreamQuota.js';

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
