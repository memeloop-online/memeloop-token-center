import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer } from './support/isolated-vite-server.js';

test('real ProvidersPage propagates saved-account read failure without conflating statistics or replaying OAuth', { timeout: 60_000 }, async () => {
  const root = fileURLToPath(new URL('..', import.meta.url));
  const server = await createIsolatedFixtureServer({ root, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'zh-CN'));
    const savedAccount = { id: 'saved-fixture-account', name: 'Fixture OAuth', driver: 'fixture-plugin', tenant_external_id: 'fixture-a', auth_kind: 'oauth', connection_method: 'oauth', status: 'active', config: {}, credential_generation: 1, route_count: 0, created_at: 1, updated_at: 1 };
    let completeCalls = 0; let failedReads = 0; let failAccounts = false;
    const mutations: string[] = [];
    await page.route('**/internal/v1/**', async route => {
      const path = new URL(route.request().url()).pathname;
      if (route.request().method() !== 'GET') mutations.push(path);
      if (path === '/internal/v1/provider-types') return route.fulfill({ json: [{
        id: 'fixture-plugin', display_name: 'Fixture OAuth', source: 'plugin', protocols: ['openai'], modalities: ['text'],
        config_schema: { type: 'object' }, credential_schema: { type: 'object', properties: { type: { const: 'oauth' } } },
        oauth_adapter: { api_version: 'oauth-adapter-v1', flow_kind: 'authorization_code_pkce', login_url: 'https://example.invalid', poll_url: '', refresh_url: 'https://example.invalid' },
      }] });
      if (path === '/internal/v1/upstreams') {
        if (failAccounts) { failedReads++; return route.fulfill({ status: 503, json: { error: { message: 'fixture account read failed' } } }); }
        return route.fulfill({ json: completeCalls ? [savedAccount] : [] });
      }
      if (path.endsWith('/authorization-code/start')) return route.fulfill({ json: { driver: 'fixture-plugin', login_url: 'https://example.invalid', session_token: 'fixture-session', expires_at: Date.now() + 600_000, recovery_expires_at: Date.now() + 87_000_000 } });
      if (path.endsWith('/authorization-code/complete')) {
        completeCalls++; failAccounts = true;
        return route.fulfill({ status: 201, json: savedAccount });
      }
      assert.equal(route.request().method(), 'GET', 'no unexpected mutation is allowed');
      if (path.includes('monitoring') || path.includes('availability')) return route.fulfill({ status: 503, json: { error: { message: 'fixture statistics failed' } } });
      return route.fulfill({ json: [] });
    });
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/authorization-code.html?full-page`);
    await page.locator('.create-journey [data-workspace-toggle]').click();
    await page.getByRole('button', { name: '账户授权', exact: true }).click();
    await page.getByRole('button', { name: '开始登录', exact: true }).click();
    await page.getByLabel('完整回调地址', { exact: true }).fill('http://localhost/cb?code=fixture&state=fixture');
    await page.getByRole('button', { name: '完成授权', exact: true }).click();
    const readFailure = page.getByText('账号已保存。重新读取账号列表即可查看。', { exact: true });
    await readFailure.waitFor();
    assert.equal(completeCalls, 1); assert.equal(failedReads, 1);
    assert.equal(await page.getByRole('alert').count(), 1, 'OAuth owns its saved-but-unread feedback without a duplicate boundary alert');
    assert.equal(await page.getByText('fixture account read failed', { exact: true }).count(), 0, 'raw account read errors stay out of the page');
    assert.equal(await page.getByRole('button', { name: '完成授权', exact: true }).count(), 0);
    const mutationsBeforeRetry = [...mutations];
    const failedRetry = page.waitForResponse(response => new URL(response.url()).pathname === '/internal/v1/upstreams' && response.request().method() === 'GET' && response.status() === 503);
    await page.getByRole('button', { name: '检查账号列表', exact: true }).click();
    await failedRetry;
    await readFailure.waitFor();
    assert.equal(failedReads, 2, 'a failed read remains retryable instead of being consumed as success');
    assert.equal(await page.getByRole('alert').count(), 1);
    assert.deepEqual(mutations, mutationsBeforeRetry, 'failed retry never replays a write or OAuth exchange');
    failAccounts = false;
    await page.getByRole('button', { name: '检查账号列表', exact: true }).click();
    await readFailure.waitFor({ state: 'detached' });
    await page.getByText('已保存上游连接 Fixture OAuth。', { exact: true }).waitFor();
    assert.equal(await page.locator('.create-journey').getAttribute('data-open'), 'false');
    await page.locator('#provider-details-saved-fixture-account').waitFor();
    assert.equal(completeCalls, 1, 'list retry and continuing statistics failures never resend the authorization code');
    assert.deepEqual(mutations, mutationsBeforeRetry, 'successful retry also performs reads only');
    assert.equal(await page.getByRole('alert').count(), 0);
  } finally { await browser.close(); await server.close(); }
});
