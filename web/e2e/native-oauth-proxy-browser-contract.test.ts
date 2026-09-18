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
  const fixtureUrl = `http://127.0.0.1:${address.port}/e2e/fixtures/authorization-code.html?full-page`;
  const browser = await chromium.launch({ headless: true });
  type Profile = { id: string; display_name: string; flow_kind: string; endpoint: string; source: 'builtin' | 'plugin'; config_schema?: Record<string, unknown> };
  const profiles: Profile[] = [
    { id: 'openai-codex', display_name: 'Codex', flow_kind: 'openai_device', endpoint: 'codex', source: 'builtin' },
    { id: 'anthropic-claude', display_name: 'Claude', flow_kind: 'claude_manual_pkce', endpoint: 'claude', source: 'builtin' },
    { id: 'cursor', display_name: 'Cursor', flow_kind: 'cursor_pkce', endpoint: 'cursor', source: 'builtin' },
    { id: 'github-copilot', display_name: 'Copilot', flow_kind: 'github_device_copilot', endpoint: 'copilot', source: 'builtin' },
    { id: 'cloud-subscription', display_name: 'Cloud subscription', flow_kind: 'cursor_pkce', endpoint: 'provider-adapter', source: 'plugin', config_schema: { type: 'object', required: ['workspace'], properties: { workspace: { type: 'string', title: 'Cloud workspace' } } } },
  ];
  const screenshotRoot = fileURLToPath(new URL('../e2e-artifacts/upstream-availability', import.meta.url));
  await mkdir(screenshotRoot, { recursive: true });
  async function open(profile: typeof profiles[number], reauthorize = false) {
    const page = await browser.newPage({ viewport: { width: 390, height: 1000 } });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'zh-CN'));
    await page.context().grantPermissions(['clipboard-read', 'clipboard-write']);
    const posts: { path: string; body: Record<string, unknown> }[] = [];
    const account = { id: 'existing-account', tenant_external_id: 'fixture-a', driver: profile.id, name: 'Existing account', auth_kind: 'oauth', connection_method: 'oauth', status: 'active', credential_generation: 2, updated_at: 3, route_count: 0, config: profile.source === 'plugin' ? { workspace: 'existing-workspace' } : { base_url: 'https://example.invalid' }, can_reauthorize: true, can_update_transport_proxy: true, has_proxy: true, proxy_scheme: 'socks5h', proxy_remote_dns: true };
    await page.route('**/internal/v1/**', async route => {
      const request = route.request(), path = new URL(request.url()).pathname;
      if (request.method() !== 'GET') {
        assert.equal(request.method(), 'POST'); assert.equal(path, `/internal/v1/oauth/${profile.endpoint}/start`);
        posts.push({ path, body: request.postDataJSON() });
        return route.fulfill({ json: { session_token: 'mock-session', login_url: `${fixtureUrl}#provider-login`, verification_url: `${fixtureUrl}#provider-login`, user_code: 'MOCK-CODE' } });
      }
      if (path === '/internal/v1/provider-types') return route.fulfill({ json: profiles.map(value => ({ ...value, protocols: ['openai'], modalities: ['text'], config_schema: value.config_schema ?? { type: 'object', properties: { base_url: { type: 'string' } } }, credential_schema: { type: 'object', properties: { type: { const: 'oauth' } } }, oauth_adapter: { api_version: 'oauth-adapter-v1', flow_kind: value.flow_kind, login_url: 'https://login.example.invalid', poll_url: 'https://poll.example.invalid', refresh_url: 'https://refresh.example.invalid' } })) });
      if (path === '/internal/v1/upstreams') return route.fulfill({ json: reauthorize ? [account] : [] });
      if (path.endsWith('/transport-proxy')) return route.fulfill({ json: { account_id: account.id, credential_generation: 2, updated_at: 3, proxy_url: 'socks5h://10.0.0.8:1080' } });
      return route.fulfill({ json: [] });
    });
    await page.goto(fixtureUrl);
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
    const directory = page.getByRole('dialog', { name: '服务提供商目录', exact: true });
    const search = directory.getByRole('combobox', { name: '搜索服务提供商', exact: true });
    assert.equal(await search.getAttribute('placeholder'), '搜索服务提供商');
    assert.equal(await directory.getByRole('group').count(), 0, 'flat provider directories omit redundant one-item groups');
    await search.fill('no-such-provider');
    await directory.getByText('试试其他服务商名称', { exact: true }).waitFor();
    await search.fill('');
    await page.getByRole('option').filter({ has: page.getByText(name, { exact: true }) }).click();
  }
  try {
    for (const profile of profiles) {
      const { page, posts } = await open(profile);
      const start = page.getByRole('button', { name: '开始登录', exact: true });
      if (profile.source === 'plugin') await page.getByLabel(/Cloud workspace/).fill('workspace-a');
      assert.equal(await start.isEnabled(), true, 'directly connected environments can start every OAuth flow without a proxy');
      await page.getByRole('checkbox', { name: '使用账号网络代理' }).check();
      const proxy = page.getByLabel(/^代理地址/);
      assert.equal(await start.isDisabled(), true);
      await proxy.fill('socks5h://8.8.8.8:1080'); assert.equal(await start.isDisabled(), true);
      await proxy.fill('socks5h://10.0.0.8:1080'); assert.equal(await start.isEnabled(), true);
      for (const width of [390, 1440]) {
        await page.setViewportSize({ width, height: 1000 });
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
        await page.screenshot({ path: `${screenshotRoot}/native-oauth-${profile.endpoint}-${width}.png` });
      }
      await start.click();
      await page.getByText('MOCK-CODE', { exact: true }).waitFor({ state: 'visible' });
      assert.equal(posts.length, 1); assert.equal(posts[0].body.proxy_url, 'socks5h://10.0.0.8:1080');
      assert.equal(posts[0].body.tenant_external_id, 'fixture-a'); assert.equal('upstream_account_id' in posts[0].body, false);
      if (profile.source === 'plugin') assert.deepEqual(posts[0].body.provider_config, { workspace: 'workspace-a' }, 'plugin fields use the shared provider configuration contract');
      const copyLogin = page.getByRole('button', { name: '复制登录地址', exact: true });
      const openLogin = page.getByRole('link', { name: '打开授权页', exact: true });
      assert.equal(await copyLogin.count(), 1, 'each flow exposes a login URL for a browser with the selected egress');
      assert.equal(await openLogin.getAttribute('target'), '_blank');
      assert.equal(await openLogin.getAttribute('href'), `${fixtureUrl}#provider-login`);
      await copyLogin.click();
      assert.equal(await page.evaluate(() => navigator.clipboard.readText()), `${fixtureUrl}#provider-login`);
      const popupPromise = page.context().waitForEvent('page');
      await openLogin.click();
      const popup = await popupPromise;
      await popup.waitForURL(/#provider-login$/);
      await popup.close();
      assert.equal(await page.locator('input').evaluateAll(inputs => inputs.some(input => input instanceof HTMLInputElement && input.value.includes('10.0.0.8'))), false, 'successful start clears the proxy draft');
      await page.close();
    }
    for (const profile of profiles) {
      const { page, posts } = await open(profile);
      assert.equal(await page.getByRole('checkbox', { name: '使用账号网络代理' }).isChecked(), false);
      await choose(page, 'Codex'); await choose(page, profile.display_name);
      if (profile.source === 'plugin') await page.getByLabel(/Cloud workspace/).fill('workspace-direct');
      assert.equal(await page.getByRole('checkbox', { name: '使用账号网络代理' }).isChecked(), false, 'switching providers clears network choice and draft');
      await page.getByRole('button', { name: '开始登录', exact: true }).click();
      await page.getByText('MOCK-CODE', { exact: true }).waitFor({ state: 'visible' });
      assert.equal(posts.length, 1); assert.equal('proxy_url' in posts[0].body, false, 'direct choice never carries a prior provider proxy'); await page.close();
      const existing = await open(profile, true);
      assert.equal(await existing.page.getByRole('checkbox', { name: '使用账号网络代理' }).count(), 0);
      await existing.page.getByRole('button', { name: '开始登录', exact: true }).click();
      await existing.page.getByText('MOCK-CODE', { exact: true }).waitFor({ state: 'visible' });
      assert.equal(existing.posts.length, 1); assert.equal(existing.posts[0].body.upstream_account_id, 'existing-account'); assert.equal('proxy_url' in existing.posts[0].body, false, 'reauthorization reuses the backend account proxy'); await existing.page.close();
    }
  } finally { await browser.close(); await server.close(); }
});
