import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium, type Page } from 'playwright';
import { createIsolatedFixtureServer } from './support/isolated-vite-server.js';

test('native OAuth creates with the chosen proxy, preserves direct choice, and reauthorizes with stored transport', { timeout: 60_000 }, async () => {
  const root = fileURLToPath(new URL('..', import.meta.url));
  const server = await createIsolatedFixtureServer({ root, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen(); const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  const profiles = [
    { id: 'openai-codex', display_name: 'Codex', flow_kind: 'openai_device', endpoint: 'codex' },
    { id: 'cursor', display_name: 'Cursor', flow_kind: 'cursor_pkce', endpoint: 'cursor' },
    { id: 'github-copilot', display_name: 'Copilot', flow_kind: 'github_device_copilot', endpoint: 'copilot' },
  ];
  const screenshotRoot = fileURLToPath(new URL('../e2e-artifacts/upstream-availability', import.meta.url));
  await mkdir(screenshotRoot, { recursive: true });
  async function open(profile: typeof profiles[number], reauthorize = false) {
    const page = await browser.newPage({ viewport: { width: 390, height: 1000 } });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'zh-CN'));
    const posts: { path: string; body: Record<string, unknown> }[] = [];
    const account = { id: 'existing-account', tenant_external_id: 'fixture-a', driver: profile.id, name: 'Existing account', auth_kind: 'oauth', connection_method: 'oauth', status: 'active', credential_generation: 2, updated_at: 3, route_count: 0, config: { base_url: 'https://example.invalid' }, can_reauthorize: true, can_update_transport_proxy: true, has_proxy: true, proxy_scheme: 'socks5h', proxy_remote_dns: true };
    await page.route('**/internal/v1/**', async route => {
      const request = route.request(), path = new URL(request.url()).pathname;
      if (request.method() !== 'GET') {
        assert.equal(request.method(), 'POST'); assert.equal(path, `/internal/v1/oauth/${profile.endpoint}/start`);
        posts.push({ path, body: request.postDataJSON() });
        return route.fulfill({ json: { session_token: 'mock-session', login_url: 'https://example.invalid', verification_url: 'https://example.invalid', user_code: 'MOCK-CODE' } });
      }
      if (path === '/internal/v1/provider-types') return route.fulfill({ json: profiles.map(value => ({ ...value, source: 'builtin', protocols: ['openai'], modalities: ['text'], config_schema: { type: 'object', properties: { base_url: { type: 'string' } } }, credential_schema: { type: 'object', properties: { type: { const: 'oauth' } } }, oauth_adapter: { flow_kind: value.flow_kind } })) });
      if (path === '/internal/v1/upstreams') return route.fulfill({ json: reauthorize ? [account] : [] });
      if (path.endsWith('/transport-proxy')) return route.fulfill({ json: { account_id: account.id, credential_generation: 2, updated_at: 3, proxy_url: 'socks5h://10.0.0.8:1080' } });
      return route.fulfill({ json: [] });
    });
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/authorization-code.html?full-page`);
    if (reauthorize) {
      await page.locator('[data-inline-edit-trigger="existing-account"]').click();
      await page.getByRole('button', { name: '重新授权', exact: true }).click();
    } else {
      await page.locator('.create-journey [data-workspace-toggle]').click();
      await page.getByRole('button', { name: '账户授权', exact: true }).click();
      await choose(page, profile.display_name);
    }
    return { page, posts };
  }
  async function choose(page: Page, name: string) {
    await page.locator('.authorization-form .model-picker-trigger').click();
    await page.getByRole('option').filter({ has: page.getByText(name, { exact: true }) }).click();
  }
  try {
    for (const profile of profiles) {
      const { page, posts } = await open(profile);
      const start = page.getByRole('button', { name: '开始登录', exact: true });
      if (profile.endpoint !== 'codex') await page.getByRole('checkbox', { name: '使用账号网络代理' }).check();
      assert.equal(await start.isDisabled(), true);
      const proxy = page.getByLabel('代理地址', { exact: true });
      await proxy.fill('socks5h://8.8.8.8:1080'); assert.equal(await start.isDisabled(), true);
      await proxy.fill('socks5h://10.0.0.8:1080'); assert.equal(await start.isEnabled(), true);
      for (const width of [390, 1440]) {
        await page.setViewportSize({ width, height: 1000 });
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
        await page.screenshot({ path: `${screenshotRoot}/native-oauth-${profile.endpoint}-${width}.png` });
      }
      await start.click(); assert.equal(posts.length, 1); assert.equal(posts[0].body.proxy_url, 'socks5h://10.0.0.8:1080');
      assert.equal(posts[0].body.tenant_external_id, 'fixture-a'); assert.equal('upstream_account_id' in posts[0].body, false);
      assert.equal(await page.locator('input').evaluateAll(inputs => inputs.some(input => input.value.includes('10.0.0.8'))), false, 'successful start clears the proxy draft');
      await page.close();
    }
    for (const profile of profiles.slice(1)) {
      const { page, posts } = await open(profile);
      await page.getByRole('checkbox', { name: '使用账号网络代理' }).check();
      await page.getByLabel('代理地址', { exact: true }).fill('socks5h://10.0.0.9:1080');
      await choose(page, 'Codex'); await choose(page, profile.display_name);
      assert.equal(await page.getByRole('checkbox', { name: '使用账号网络代理' }).isChecked(), false, 'switching providers clears network choice and draft');
      await page.getByRole('button', { name: '开始登录', exact: true }).click();
      assert.equal(posts.length, 1); assert.equal('proxy_url' in posts[0].body, false, 'direct choice never carries a prior provider proxy'); await page.close();
      const existing = await open(profile, true);
      assert.equal(await existing.page.getByRole('checkbox', { name: '使用账号网络代理' }).count(), 0);
      await existing.page.getByRole('button', { name: '开始登录', exact: true }).click();
      assert.equal(existing.posts.length, 1); assert.equal(existing.posts[0].body.upstream_account_id, 'existing-account'); assert.equal('proxy_url' in existing.posts[0].body, false, 'reauthorization reuses the backend account proxy'); await existing.page.close();
    }
  } finally { await browser.close(); await server.close(); }
});
