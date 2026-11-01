import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer } from './support/isolated-vite-server.js';
import { manageProviderAccount } from './support/provider-account-navigation.js';
import { quotaBatchFixtureResponse, quotaReadFixtureResponse } from './support/quota-batch.js';
import type { UpstreamQuotaSnapshot } from '../src/operator/upstreamQuota.js';

test('account recovery uses the production shell and workspace with contained Fluent controls', { timeout: 120_000 }, async () => {
  const root = fileURLToPath(new URL('..', import.meta.url));
  const server = await createIsolatedFixtureServer({ root, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const origin = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'zh-CN'));
    const now = Date.now();
    const renewal = { credential_generation: 3, access_state: 'expired', refresh_state: 'outcome_unknown', failure_class: 'network', reauthorization_required: false, automatic_refresh_paused: true };
    const accounts = [
      { id: 'kimi-0142', name: 'Kimi 0142', driver: 'kimi-oauth', oauth_refresh: renewal },
      { id: 'kimi-7021', name: 'Kimi 7021', driver: 'kimi-oauth', oauth_refresh: { ...renewal, failure_class: 'local_persistence' } },
      { id: 'codex-recovery', name: 'Codex 主账号', driver: 'openai-codex', oauth_refresh: undefined },
    ].map(account => ({ ...account, tenant_id: 'fixture-tenant', tenant_external_id: 'fixture-a', status: 'active', auth_kind: 'oauth', connection_method: 'oauth', credential_generation: 3, credential_expires_at: account.driver === 'kimi-oauth' ? now - 1000 : now + 3600_000, can_refresh: true, can_rotate: false, can_reauthorize: true, can_update_transport_proxy: true, route_count: 0, config: {}, created_at: 1, updated_at: 3 }));
    const eligible = accounts.map(account => ({ id: account.id, tenant: account.tenant_external_id, driver: account.driver }));
    const providers = ['kimi-oauth', 'openai-codex'].map(driver => ({ id: driver, display_name: driver === 'kimi-oauth' ? 'Kimi' : 'OpenAI Codex', source: 'builtin', protocols: ['openai'], modalities: ['text'], config_schema: { type: 'object', properties: {} }, credential_schema: { type: 'object', properties: { type: { const: 'oauth' } } }, oauth_adapter: { flow_kind: driver === 'kimi-oauth' ? 'kimi_device' : 'openai_device' } }));
    const snapshot = (value: UpstreamQuotaSnapshot): UpstreamQuotaSnapshot => value.provider === 'openai-codex' ? {
      ...value,
      windows: value.windows.map(window => ({ ...window, id: 'code:primary_window', source: 'codex_usage', period_seconds: 18000 })),
      reset_capability: { ...value.reset_capability, provider_supported: true, implementation_available: true, prepare_available: true, confirmation_required: true, available_credits: 1, applicable_credits: 1 },
    } : { ...value, windows: value.windows.map(window => ({ ...window, id: 'limit-0', source: 'kimi_usage', period_seconds: 18000 })) };
    let operation = { id: 'reviewed-operation', upstream_account_id: 'codex-recovery', state: 'unknown', expires_at: now, effect: 'supplier_defined_codex_rate_limits', consumes_credits: 1, last_reconciled_at: null as number | null, reconciled_available_credits: null as number | null, reconciled_applicable_credits: null as number | null, settled_at: null as number | null, updated_at: now, manual_unlock_available: true, quota_refresh_status: 'not_attempted' };
    const recoveries: unknown[] = [];
    const errors: string[] = [];
    const unexpected: string[] = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.route('**/*', async route => {
      const request = route.request();
      const url = new URL(request.url());
      if (url.origin !== origin) { unexpected.push(url.origin); return route.abort(); }
      if (!url.pathname.startsWith('/internal/')) return route.continue();
      if (request.method() === 'POST' && url.pathname === '/internal/v1/upstreams/quota/batch') {
        const result = quotaBatchFixtureResponse(request.postDataJSON(), eligible);
        return route.fulfill({ json: { ...result, results: result.results.map(entry => entry.status === 'success' ? { ...entry, snapshot: snapshot(entry.snapshot) } : entry) } });
      }
      if (request.method() === 'POST' && url.pathname === '/internal/v1/upstreams/codex-recovery/quota-reset/reviewed-operation/recover') {
        assert.equal(url.searchParams.get('tenant_external_id'), 'fixture-a');
        assert.deepEqual(request.postDataJSON(), { expected_updated_at: now, reason: 'supplier_confirmed_not_applied' });
        recoveries.push(request.postDataJSON());
        operation = { ...operation, settled_at: now + 1, updated_at: now + 1, manual_unlock_available: false, last_reconciled_at: now + 1, reconciled_available_credits: 1, reconciled_applicable_credits: 1 };
        return route.fulfill({ json: operation });
      }
      if (request.method() !== 'GET') { unexpected.push(`${request.method()} ${url.pathname}`); return route.abort(); }
      if (url.pathname === '/internal/v1/upstreams') return route.fulfill({ json: accounts });
      if (url.pathname === '/internal/v1/provider-types') return route.fulfill({ json: providers });
      if (/^\/internal\/v1\/upstreams\/[^/]+\/quota$/.test(url.pathname)) return route.fulfill({ json: snapshot(quotaReadFixtureResponse(url, eligible)) });
      if (url.pathname.endsWith('/quota-reset/current')) return route.fulfill({ json: operation });
      if (url.pathname.endsWith('/models')) return route.fulfill({ json: { account_id: url.pathname.split('/')[4], credential_generation: 3, status: 'ready', models: [], disabled_models: [], last_attempt_at: now, last_success_at: now, expires_at: now + 3600_000, error_code: null } });
      if (url.pathname.endsWith('/transport-proxy')) return route.fulfill({ json: { account_id: url.pathname.split('/')[4], credential_generation: 3, updated_at: 3, proxy_url: null } });
      if (url.pathname.endsWith('/access')) return route.fulfill({ json: { can_manage: false } });
      if (url.pathname.includes('monitoring')) return route.fulfill({ json: { top_upstream_models: [] } });
      if (url.pathname.includes('availability')) return route.fulfill({ json: { tenant_external_id: 'fixture-a', accounts: [] } });
      return route.fulfill({ json: [] });
    });
    await page.goto(`${origin}/e2e/fixtures/authorization-code.html?full-page&app-shell`);
    await page.locator('.app-context-bar').waitFor();
    const artifacts = join(root, 'e2e-artifacts', 'upstream-quota');
    await mkdir(artifacts, { recursive: true });
    for (const [theme, width] of [['light', 1440], ['dark', 390]] as const) {
      await page.evaluate(value => { document.documentElement.dataset.theme = value; }, theme);
      await page.setViewportSize({ width, height: 1000 });
      const directory = page.locator('.provider-list');
      if (!await directory.isVisible()) await page.locator('.provider-detail-workspace').getByRole('button', { name: '返回账号列表', exact: true }).click();
      for (const accountId of ['kimi-0142', 'kimi-7021']) {
        const row = page.locator(`[data-upstream-id="${accountId}"]`);
        assert.equal(await row.locator('.status').filter({ hasText: '续期结果待核对' }).innerText(), '续期结果待核对');
        assert.equal(await row.getByRole('button', { name: '重新登录', exact: true }).isEnabled(), true);
      }
      await page.screenshot({ path: join(artifacts, `kimi-workspace-directory-${theme}-${width}.png`), fullPage: true });
      for (const [accountId, reason] of [['kimi-0142', /网络读取失败/], ['kimi-7021', /续期结果未能保存/]] as const) {
        const workspace = await manageProviderAccount(page, accountId);
        const badge = workspace.locator('.account-main > .status');
        assert.equal(await badge.innerText(), '续期结果待核对');
        assert.equal(await workspace.getByRole('button', { name: '重新登录', exact: true }).isVisible(), true);
        await badge.focus();
        await page.getByRole('tooltip').filter({ hasText: reason }).waitFor();
        assert.match(await page.getByRole('tooltip').innerText(), /访问令牌已过期.*自动续期已暂停/);
        await badge.press('Tab');
        await page.getByRole('tooltip').waitFor({ state: 'hidden' });
        await page.screenshot({ path: join(artifacts, `kimi-workspace-${accountId}-${theme}-${width}.png`), fullPage: true });
      }
      const workspace = await manageProviderAccount(page, 'codex-recovery');
      const recovery = workspace.getByRole('button', { name: '核对并恢复', exact: true });
      await recovery.waitFor();
      const reason = workspace.getByRole('combobox', { name: '恢复依据', exact: true });
      assert.equal(await reason.inputValue(), 'operator_authorized_retry');
      assert.equal(await reason.locator('option').allTextContents().then(labels => labels.join('|')), '人工恢复|上游已确认未执行');
      const select = await reason.boundingBox();
      const icon = await workspace.locator('.fui-Select__icon').boundingBox();
      const button = await recovery.boundingBox();
      assert.ok(select && icon && button);
      assert.ok(icon.y >= select.y && icon.y + icon.height <= select.y + select.height, 'caret stays inside the select row');
      assert.ok(icon.x >= select.x && icon.x + icon.width <= select.x + select.width, 'caret stays inside the select width');
      assert.ok(button.y >= select.y + select.height, 'action stays in its own row');
      assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth));
      assert.equal(await workspace.getByRole('checkbox').count(), 0);
      assert.equal(recoveries.length, 0);
      await page.screenshot({ path: join(artifacts, `kimi-workspace-recovery-${theme}-${width}.png`), fullPage: true });
    }
    const workspace = page.locator('.provider-detail-workspace');
    await workspace.getByRole('combobox', { name: '恢复依据', exact: true }).selectOption('supplier_confirmed_not_applied');
    await workspace.getByRole('button', { name: '核对并恢复', exact: true }).click();
    await workspace.getByText('已恢复操作，上次重置结果仍待核对。', { exact: true }).waitFor();
    assert.equal(recoveries.length, 1);
    await workspace.getByRole('button', { name: '上次操作详情', exact: true }).click();
    await workspace.getByText(/本次操作锁定已解除，上次重置结果仍未知/).waitFor();
    await page.screenshot({ path: join(artifacts, 'kimi-workspace-released-dark-390.png'), fullPage: true });
    assert.deepEqual(unexpected, []);
    assert.deepEqual(errors, []);
  } finally { await browser.close(); await server.close(); }
});
