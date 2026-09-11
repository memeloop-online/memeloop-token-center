import assert from 'node:assert/strict';
import test from 'node:test';
import { chromium } from 'playwright';

// Run against a local Vite server. Every API request is fulfilled here; no upstream traffic is possible.
test('upstream connection, OAuth and quota UX in Chromium with isolated API mocks', { skip: !process.env.MTC_UX_BASE_URL }, async () => {
  const base = process.env.MTC_UX_BASE_URL!;
  assert.match(base, /^http:\/\/127\.0\.0\.1:\d+$/);
  const browser = await chromium.launch({ headless: true });
  const page = await browser.newPage({ viewport: { width: 1440, height: 1080 } });
  page.setDefaultTimeout(120_000);
  const requests: string[] = [];
  const errors: string[] = [];
  page.on('pageerror', (error) => errors.push(error.message));
  const account = {
    id: 'mock-codex', tenant_id: 'mock', tenant_external_id: 'default', name: 'Codex primary',
    driver: 'openai-codex', auth_kind: 'oauth', connection_method: 'oauth', credential_generation: 1,
    status: 'active', credential_expires_at: null, can_refresh: true, can_rotate: false, can_reauthorize: true,
    route_count: 0, config: { base_url: 'https://chatgpt.com/backend-api/codex' }, created_at: 1, updated_at: 1,
    has_proxy: true, proxy_scheme: 'socks5h', proxy_remote_dns: true,
  };
  const provider = {
    id: 'openai-codex', display_name: 'Codex', source: 'builtin', protocols: ['openai'], modalities: ['text'],
    config_schema: { type: 'object', properties: { base_url: { type: 'string', const: account.config.base_url, default: account.config.base_url, readOnly: true } } },
    credential_schema: { type: 'object', properties: { type: { const: 'oauth' }, access_token: { type: 'string' } } },
    oauth_adapter: { flow_kind: 'openai_device' },
  };
  const directProvider = {
    id: 'http-json', display_name: 'HTTP API', source: 'builtin', protocols: ['openai'], modalities: ['text'],
    config_schema: { type: 'object', required: ['base_url'], properties: { base_url: { type: 'string', default: 'https://example.test/v1' } } },
    credential_schema: { oneOf: [{ title: 'API key', type: 'object', required: ['type', 'value'], properties: { type: { const: 'api_key', default: 'api_key' }, value: { type: 'string' } } }] },
  };
  let quotaReads = 0;
  let confirmationAttempts = 0;
  let confirmationKey = '';
  let confirmationBody = '';
  const quota = {
    contract_version: 'upstream_quota_v1', upstream_account_id: account.id, tenant_external_id: 'default',
    provider: 'openai-codex', status: 'ready', observed_at: Date.now(), stale_after: Date.now() + 60000, stale: false,
    plan_type: 'Plus', credits: { balance: null, unlimited: false, has_credits: true },
    windows: [{ id: 'weekly', label: 'Weekly', used_percent: 83, remaining: null, limit: null, reset_at: null, period_seconds: 604800, source: 'mock', reset_is_estimated: false, allowed: true, limit_reached: false }],
    reset_capability: { provider_supported: true, implementation_available: true, available_credits: 2, applicable_credits: 1, reason: null, credit_error_code: null },
    error_code: null,
  };
  const operation = { id: 'mock-operation', upstream_account_id: account.id, state: 'prepared', expires_at: Date.now() + 60000, effect: 'supplier_defined_codex_rate_limits', consumes_credits: 1, last_reconciled_at: null, reconciled_available_credits: null, reconciled_applicable_credits: null };
  await page.route('**/*', async (route) => {
    const url = new URL(route.request().url());
    if (url.origin !== base) return route.abort();
    if (url.pathname === '/operator') return route.fulfill({ response: await route.fetch({ url: `${base}/ui-assets/` }) });
    if (!url.pathname.startsWith('/internal/')) return route.continue();
    const path = url.pathname; requests.push(`${route.request().method()} ${path}`);
    let body: unknown = [];
    let status = 200;
    if (path.endsWith('/tenants')) body = [{ id: 'mock', external_id: 'default', name: 'Mock tenant' }];
    else if (path.endsWith('/provider-types')) body = [provider, directProvider];
    else if (path.endsWith('/upstreams')) {
      if (route.request().method() === 'POST') {
        assert.equal(route.request().postDataJSON().name, 'New mock API');
        body = account;
      } else body = [account];
    }
    else if (path.endsWith('/monitoring-snapshot')) body = { top_upstream_models: [] };
    else if (path.endsWith('/upstream-availability')) body = { contract_version: 'upstream_account_availability_v1', tenant_external_id: 'default', generated_at: Date.now(), from_created_at: Date.now() - 86400000, to_created_at: Date.now(), accounts: [{ upstream_account_id: account.id, metrics: { requests: 0, successful_requests: 0, failed_requests: 0, avg_duration_ms: null, p95_duration_ms: null }, terminal_outcomes: [] }] };
    else if (path.endsWith('/transport-proxy')) {
      const payload = route.request().postDataJSON();
      assert.equal(payload.proxy_url, 'socks5h://10.0.0.10:1080');
      assert.equal(payload.expected_credential_generation, 1);
      assert.ok(route.request().headers()['idempotency-key']);
      body = account;
    } else if (path.endsWith('/quota')) {
      quotaReads += 1;
      if (quotaReads > 1) { status = 503; body = { error: { message: 'Mock quota refresh failed' } }; }
      else body = quota;
    } else if (path.endsWith('/quota-reset/prepare')) body = { operation, confirmation_token: 'mock-only' };
    else if (path.endsWith('/mock-operation/confirm')) {
      confirmationAttempts += 1;
      const key = route.request().headers()['idempotency-key'];
      const payload = route.request().postData()!;
      assert.equal(route.request().postDataJSON().confirmation, 'consume_one_supplier_reset_credit');
      if (confirmationAttempts === 1) { confirmationKey = key; confirmationBody = payload; status = 503; body = { error: { message: 'Mock unknown result' } }; }
      else { assert.equal(key, confirmationKey); assert.equal(payload, confirmationBody); body = { ...operation, state: 'accepted' }; }
    }
    else if (path.endsWith('/health')) body = { account_id: account.id, status: 'healthy', checked_at: Date.now(), latency_ms: 42 };
    else if (path.endsWith('/oauth/codex/start')) {
      assert.equal(route.request().postDataJSON().proxy_url, 'socks5h://10.0.0.10:1080');
      body = { session_token: 'mock-session', verification_url: 'https://example.test/authorize', user_code: 'MOCK-CODE' };
    } else if (path.endsWith('/oauth/codex/poll')) body = { status: 'pending' };
    else if (path === '/internal/v1/upstreams/mock-codex' && route.request().method() === 'PUT') body = account;
    return route.fulfill({ status, json: body });
  });
  await page.addInitScript(() => {
    localStorage.setItem('mtc.operator.service-credential.v1', 'mock-only');
    localStorage.setItem('mtc.operator.tenant.v1', 'default');
    localStorage.setItem('mtc-locale', 'en');
  });
  try {
    await page.goto(`${base}/operator?view=providers`);
    const card = page.locator('.provider-account[data-upstream-id="mock-codex"]');
    await card.waitFor();
    assert.equal(requests.some((value) => /\/(quota|health|models|refresh|prepare|confirm)$/.test(value)), false);
    // Locale-independent selectors for main connection controls; snapshots capture the real localized UI.
    const connection = card.locator('.upstream-connection');
    await connection.getByRole('button').click();
    const input = connection.locator('input');
    await input.fill('https://not-a-proxy.example');
    assert.equal(await connection.getByRole('button', { name: /保存网络代理|Save network proxy/ }).isDisabled(), true);
    await input.fill('socks5h://10.0.0.10:1080');
    await connection.getByRole('button', { name: /保存网络代理|Save network proxy/ }).click();
    await connection.getByRole('status').waitFor();
    await card.locator('.upstream-secondary-actions > summary').click();
    await card.getByRole('button', { name: /编辑上游|Edit upstream|^编辑$|^Edit$/ }).click();
    const editor = page.locator('.inline-editor');
    assert.equal(await editor.locator('input[readonly]').count() > 0, true);
    await editor.locator('#root_name').fill('Renamed mock');
    await editor.getByRole('button', { name: /保存|Save/ }).click();
    await editor.waitFor({ state: 'hidden' });
    // Acceptance boundary: do not activate quota, reset, health or model sync controls,
    // even with mocked transport. Quota visuals are covered by a static-only fixture.
    assert.equal(quotaReads, 0);
    assert.equal(confirmationAttempts, 0);
    await card.locator('.upstream-secondary-actions > summary').click();
    const onboarding = page.locator('.provider-onboarding');
    await onboarding.locator('summary').click();
    await onboarding.locator('#root_name').fill('New mock API');
    await onboarding.locator('#root_credential_value').fill('mock-api-key');
    await Promise.all([
      page.waitForResponse((response) => response.url().endsWith('/internal/v1/upstreams') && response.request().method() === 'POST'),
      onboarding.getByRole('button', { name: /添加上游|Add upstream/ }).click(),
    ]);
    assert.ok(requests.includes('POST /internal/v1/upstreams'));
    await onboarding.getByRole('button', { name: /账户授权|Account authorization|OAuth/ }).click();
    const start = onboarding.getByRole('button', { name: /开始登录|Start login/ });
    assert.equal(await start.isDisabled(), true);
    await page.evaluate(() => window.scrollTo(0, 0));
    await page.screenshot({ path: '/tmp/mtc-upstream-ux-oauth-before-login-desktop.png', fullPage: true, animations: 'disabled' });
    await page.setViewportSize({ width: 390, height: 844 });
    await page.waitForFunction(() => document.querySelector('.app-sidebar')!.getBoundingClientRect().right <= 0);
    await page.screenshot({ path: '/tmp/mtc-upstream-ux-oauth-before-login-mobile.png', fullPage: true, animations: 'disabled' });
    await page.setViewportSize({ width: 1440, height: 1080 });
    await onboarding.locator('.upstream-proxy-editor input').fill('socks5h://10.0.0.10:1080');
    await start.click();
    await onboarding.locator('.device-authorization').waitFor();
    await onboarding.getByRole('button', { name: /检查授权结果|Check authorization/ }).click();
    await page.evaluate(() => window.scrollTo(0, 0));
    await page.screenshot({ path: '/tmp/mtc-upstream-ux-desktop.png', fullPage: true });
    await page.setViewportSize({ width: 390, height: 844 });
    await page.waitForFunction(() => document.querySelector('.app-sidebar')!.getBoundingClientRect().right <= 0);
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth), true);
    await page.screenshot({ path: '/tmp/mtc-upstream-ux-mobile.png', fullPage: true, animations: 'disabled' });
    assert.deepEqual(errors, []);
    assert.equal(requests.some((value) => /\/(quota|health|models|refresh|prepare|confirm)$/.test(value)), false);
  } catch (error) {
    console.error({ errors, requests, text: (await page.locator('body').innerText()).slice(-4000) });
    throw error;
  } finally { await browser.close(); }
});
