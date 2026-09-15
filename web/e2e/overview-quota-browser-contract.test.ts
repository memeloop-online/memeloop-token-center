import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer as createServer } from './support/isolated-vite-server.js';
import { fixtureAssets } from './support/fixture-assets.js';
import type { UpstreamQuotaSnapshot } from '../src/operator/upstreamQuota.js';

test('production overview reads each leading account once and preserves unknown and historical quota', { timeout: 60_000 }, async () => {
  const root = fileURLToPath(new URL('..', import.meta.url));
  const server = await createServer({ root, configFile: false, plugins: [fixtureAssets()], logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen(); const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const origin = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  const now = Date.UTC(2026, 8, 15, 12);
  const metrics = { requests: 32, successful_requests: 28, failed_requests: 4, avg_duration_ms: 180, p95_duration_ms: 420, costs: [] };
  const health = { version: 'upstream_breaker_v1', status: 'unknown', observed_at: now };
  const snapshot = { contract_version: 'v1', generated_at: now, scope: 'tenant', tenant_external_id: 'default',
    from_created_at: now - 86_400_000, to_created_at: now, granularity: 'hour', latency_is_approximate: true, latency_method: 'fixed_histogram_upper_bound_capped_60000ms',
    summary: metrics, freshness: { latest_terminal_created_at: now, age_millis: 0 }, health,
    top_upstream_models: [
      { upstream_account_id: 'known', upstream_name: 'Research subscription', model: 'model-a', metrics, health, terminal_outcomes: [] },
      { upstream_account_id: 'known', upstream_name: 'Research subscription', model: 'model-b', metrics, health, terminal_outcomes: [] },
      { upstream_account_id: 'unknown', upstream_name: 'No observation yet', model: 'model-a', metrics, health, terminal_outcomes: [] },
    ],
  };
  const window = { id: 'code:primary_window', label: 'code:primary_window', used_percent: 51, used: null, remaining: null, limit: null, unit: null, reset_at: null, period_seconds: 18_000, source: 'codex_usage', reset_is_estimated: false, allowed: true, limit_reached: false };
  const quota: UpstreamQuotaSnapshot = {
    contract_version: 'upstream_quota_v1', upstream_account_id: 'known', tenant_external_id: 'default', provider: 'openai-codex', status: 'ready', observed_at: now, stale_after: now + 30_000, stale: false, freshness: 'fresh', plan_type: null, workspace: null,
    capabilities: { read: true, plan: false, workspace: false, window_amounts: false, window_amount_unit: false, window_percent: true, reset_credit_expiry: false, subscription_expiry: false, supplier_read_only: true, refreshes_credentials: false, consumes_reset_credit: false },
    subscription_active_until: null, credits: { balance: null, unlimited: null, has_credits: null, source: null }, reset_credits: [], error_code: null,
    windows: [window, { ...window, id: 'code:secondary_window', period_seconds: 604_800, used_percent: 20 }, { ...window, id: 'code_review:primary_window', used_percent: null }],
    reset_capability: { provider_supported: false, implementation_available: false, prepare_available: false, confirmation_required: false, retryable: false, available_credits: null, applicable_credits: null, reason: 'unsupported', credit_error_code: null, evidence: 'server_driver_contract' },
  };
  try {
    const page = await browser.newPage();
    const unexpected: string[] = [], reads: string[] = [], errors: string[] = [];
    let failRefresh = false;
    page.on('pageerror', error => errors.push(error.message));
    await page.route('**/*', route => {
      const request = route.request(), url = new URL(request.url());
      if (url.origin !== origin || request.method() !== 'GET') { unexpected.push(`${request.method()} ${url.pathname}`); return route.abort(); }
      if (!url.pathname.startsWith('/internal/')) return route.continue();
      const json = (value: unknown) => route.fulfill({ contentType: 'application/json', body: JSON.stringify(value) });
      if (url.pathname === '/internal/v1/tenants') return json([{ id: 'synthetic', external_id: 'default', name: 'Design acceptance' }]);
      if (url.pathname === '/internal/v1/plugins' || url.pathname === '/internal/v1/requests') return json([]);
      if (url.pathname === '/internal/v1/monitoring-snapshot') return json(snapshot);
      if (url.pathname === '/internal/v1/usage-analysis/trends') return json({ from_created_at: now - 86_400_000, to_created_at: now, granularity: 'hour', time_zone: 'UTC', p95_is_approximate: true, p95_method: 'fixed_histogram_upper_bound_capped_60000ms', summary: { ...metrics, success: 28, failed: 4, input_tokens: 0, output_tokens: 0, cached_input_tokens: 0, cache_write_tokens: 0, generation_units: 0 }, time_series: [] });
      if (url.pathname === '/internal/v1/upstreams') return json(['known', 'unknown', 'not-in-overview'].map(id => ({ id, name: id, tenant_external_id: 'default', credential_generation: 1, status: 'active' })));
      if (/^\/internal\/v1\/upstreams\/(known|unknown)\/quota$/.test(url.pathname)) {
        assert.equal(url.searchParams.get('tenant_external_id'), 'default');
        const id = url.pathname.split('/')[4]; reads.push(id);
        if (id === 'known' && failRefresh) return route.fulfill({ status: 503, contentType: 'application/json', body: JSON.stringify({ error: { message: 'Synthetic read unavailable' } }) });
        return json(id === 'known' ? quota : { ...quota, upstream_account_id: 'unknown', status: 'error', observed_at: null, stale_after: null, freshness: 'unobserved', error_code: 'quota_transport_failed', windows: [{ ...window, used_percent: 0 }] });
      }
      unexpected.push(url.pathname); return route.abort();
    });
    await page.addInitScript(() => { localStorage.setItem('mtc-locale', 'en'); localStorage.setItem('mtc.operator.service-credential.v1', 'synthetic-browser-only'); });
    await page.clock.setFixedTime(now);
    await page.goto(`${origin}/operator?view=overview`);
    const section = page.getByRole('region', { name: 'Remaining quota for leading upstreams' });
    await section.getByText('Remaining 49%', { exact: true }).waitFor();
    await section.getByRole('button', { name: 'Refresh quota', exact: true }).waitFor();
    assert.equal(await section.locator('.analytics-metric').count(), 2);
    assert.equal(await section.getByRole('meter').count(), 1);
    assert.equal(await section.getByRole('meter').getAttribute('aria-valuenow'), '49');
    assert.equal(await section.getByText('Remaining 100%', { exact: true }).count(), 0);
    assert.deepEqual([...reads].sort(), ['known', 'unknown']);
    const artifacts = `${root}/e2e-artifacts/ui-system/overview-quota`; await mkdir(artifacts, { recursive: true });
    for (const theme of ['light', 'dark']) for (const width of [390, 1440]) {
      await page.evaluate(value => { document.documentElement.dataset.theme = value; }, theme);
      await page.setViewportSize({ width, height: 1000 });
      await page.evaluate(() => window.scrollTo({ top: 0, behavior: 'instant' }));
      await page.screenshot({ path: `${artifacts}/overview-${theme}-${width}.png`, fullPage: true });
      const trigger = section.locator('.quota-summary').first();
      await trigger.focus();
      const tooltip = page.getByRole('tooltip'); await tooltip.waitFor();
      assert.ok((await tooltip.innerText()).includes('51% used'));
      assert.ok((await tooltip.innerText()).includes('20% used'));
      assert.ok((await tooltip.innerText()).includes('Remaining quota unknown'));
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false);
      await page.screenshot({ path: `${artifacts}/quota-detail-${theme}-${width}.png` });
      await page.keyboard.press('Escape');
      await section.getByRole('button', { name: 'Refresh quota', exact: true }).focus();
    }
    assert.deepEqual([...reads].sort(), ['known', 'unknown'], 'focus and viewport changes do not poll suppliers');
    failRefresh = true;
    await section.getByRole('button', { name: 'Refresh quota', exact: true }).click();
    await section.locator('.quota-summary-provenance').waitFor();
    assert.ok((await section.innerText()).includes('Remaining 49%'));
    assert.ok((await section.locator('.quota-summary-provenance').innerText()).includes('51%'));
    assert.deepEqual(unexpected, []); assert.deepEqual(errors, []);
  } finally { await browser.close(); await server.close(); }
});
