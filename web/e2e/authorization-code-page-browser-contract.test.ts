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
    let completeCalls = 0; let failedReads = 0; let failAccounts = false;
    await page.route('**/internal/v1/**', async route => {
      const path = new URL(route.request().url()).pathname;
      if (path === '/internal/v1/provider-types') return route.fulfill({ json: [{
        id: 'fixture-plugin', display_name: 'Fixture OAuth', source: 'plugin', protocols: ['openai'], modalities: ['text'],
        config_schema: { type: 'object' }, credential_schema: { type: 'object', properties: { type: { const: 'oauth' } } },
        oauth_adapter: { api_version: 'oauth-adapter-v1', flow_kind: 'authorization_code_pkce', login_url: 'https://example.invalid', poll_url: '', refresh_url: 'https://example.invalid' },
      }] });
      if (path === '/internal/v1/upstreams') {
        if (failAccounts) { failedReads++; return route.fulfill({ status: 503, json: { error: { message: 'fixture account read failed' } } }); }
        return route.fulfill({ json: [] });
      }
      if (path.endsWith('/authorization-code/start')) return route.fulfill({ json: { driver: 'fixture-plugin', login_url: 'https://example.invalid', session_token: 'fixture-session', expires_at: Date.now() + 600_000, recovery_expires_at: Date.now() + 87_000_000 } });
      if (path.endsWith('/authorization-code/complete')) {
        completeCalls++; failAccounts = true;
        return route.fulfill({ status: 201, json: { id: 'saved-fixture-account', name: 'Fixture OAuth' } });
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
    assert.equal(await page.getByRole('button', { name: '完成授权', exact: true }).count(), 0);
    failAccounts = false;
    await page.getByRole('button', { name: '检查账号列表', exact: true }).click();
    await readFailure.waitFor({ state: 'detached' });
    await page.getByText('账号已创建；可在上游列表查看状态和代理配置。', { exact: true }).waitFor();
    assert.equal(completeCalls, 1, 'list retry and continuing statistics failures never resend the authorization code');
  } finally { await browser.close(); await server.close(); }
});
