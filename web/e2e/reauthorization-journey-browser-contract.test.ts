import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer } from './support/isolated-vite-server.js';

test('reauthorization saves its proxy in place, copies device codes, polls automatically and returns one level', { timeout: 120_000 }, async () => {
  const server = await createIsolatedFixtureServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen(); const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const origin = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  try {
    for (const locale of ['zh-CN', 'en'] as const) {
      const chinese = locale === 'zh-CN';
      const page = await browser.newPage({ viewport: { width: 390, height: 1000 } });
      await page.clock.install();
      await page.context().grantPermissions(['clipboard-read', 'clipboard-write']);
      await page.addInitScript(value => localStorage.setItem('mtc-locale', value), locale);
      let account = { id: 'reauthorization-fixture', name: 'reauthorize@example.org', tenant_external_id: 'fixture-a', driver: 'openai-codex', auth_kind: 'oauth', connection_method: 'oauth', status: 'active', credential_generation: 2, credential_expires_at: null, updated_at: 3, route_count: 0, config: { base_url: 'https://provider.example.invalid' }, can_reauthorize: true, can_update_transport_proxy: true, has_proxy: true, proxy_scheme: 'socks5h', proxy_remote_dns: true };
      let proxy = 'socks5h://10.0.0.8:1080';
      let pollCount = 0;
      let failPoll = true;
      let expiredRecovery = false;
      const writes: { path: string; body: Record<string, unknown> }[] = [];
      await page.route('**/*', async route => {
        const request = route.request(); const url = new URL(request.url());
        assert.equal(url.origin, origin, 'no live OAuth or external account traffic');
        if (!url.pathname.startsWith('/internal/')) return route.continue();
        if (request.method() === 'GET') {
          if (url.pathname.includes('monitoring') || url.pathname.includes('availability')) return route.fulfill({ status: 503, json: { error: { message: 'Fixture statistics unavailable' } } });
          if (url.pathname === '/internal/v1/provider-types') return route.fulfill({ json: [{ id: 'openai-codex', display_name: 'Fixture Codex', source: 'builtin', protocols: ['openai'], modalities: ['text'], config_schema: { type: 'object', properties: { base_url: { type: 'string' } } }, credential_schema: { type: 'object', properties: { type: { const: 'oauth' } } }, oauth_adapter: { flow_kind: 'openai_device' } }] });
          if (url.pathname === '/internal/v1/upstreams') return route.fulfill({ json: [account] });
          if (url.pathname.endsWith('/transport-proxy')) return route.fulfill({ json: { account_id: account.id, credential_generation: account.credential_generation, updated_at: account.updated_at, proxy_url: proxy } });
          return route.fulfill({ json: [] });
        }
        const body = request.postDataJSON(); writes.push({ path: url.pathname, body });
        if (url.pathname.endsWith('/transport-proxy')) {
          assert.equal(request.method(), 'PUT');
          assert.equal(body.expected_credential_generation, 2); assert.equal(body.expected_updated_at, 3);
          proxy = body.proxy_url;
          account = { ...account, credential_generation: 3, updated_at: 4 };
          return route.fulfill({ json: account });
        }
        assert.equal(request.method(), 'POST');
        if (url.pathname === '/internal/v1/oauth/codex/start') {
          assert.equal(body.upstream_account_id, account.id);
          assert.equal('proxy_url' in body, false);
          return route.fulfill({ json: { session_id: '00000000-0000-4000-8000-000000000001', session_token: 'synthetic-device-session', user_code: 'FIXTURE-CODE', verification_url: `${origin}/mock-provider`, poll_after_seconds: 5, expires_at: Date.now() + 600_000 } });
        }
        assert.equal(url.pathname, '/internal/v1/oauth/codex/poll');
        assert.deepEqual(body, { session_id: '00000000-0000-4000-8000-000000000001' });
        pollCount += 1;
        if (expiredRecovery) return route.fulfill({ status: 400, json: { error: { message: 'OAuth login expired or failed' } } });
        if (failPoll) { failPoll = false; return route.fulfill({ status: 503, json: { error: { message: 'do-not-display-raw-provider-error' } } }); }
        if (pollCount === 2) return route.fulfill({ status: 202, json: { status: 'pending', retry_after_seconds: 10 } });
        account = { ...account, credential_generation: 4, updated_at: 5 };
        return route.fulfill({ json: account });
      });
      await page.goto(`${origin}/e2e/fixtures/authorization-code.html?full-page`);
      await page.locator('[data-inline-edit-trigger="reauthorization-fixture"]').click();
      const reauthorize = page.getByRole('button', { name: chinese ? '重新授权' : 'Authorize again', exact: true });
      await reauthorize.click();
      const workspace = page.locator('.provider-reauthorization-workspace');
      const start = workspace.getByRole('button', { name: chinese ? '开始登录' : 'Start login', exact: true });
      assert.equal(writes.length, 0, 'opening an already-authorized account never starts OAuth');
      await workspace.getByRole('button', { name: chinese ? '配置网络代理' : 'Configure network proxy', exact: true }).click();
      assert.equal(await start.isDisabled(), true);
      await workspace.getByText(chinese ? '正在编辑网络代理。请先保存或取消代理编辑，再开始登录。' : 'You are editing the network proxy. Save or cancel proxy editing before starting login.', { exact: true }).waitFor();
      const proxyInput = workspace.locator('.upstream-proxy-editor input');
      await proxyInput.fill('socks5h://10.0.0.9:1080');
      await proxyInput.press('Enter');
      await workspace.locator('.provider-readable-proxy input').waitFor();
      assert.equal(await workspace.getByText(chinese ? '正在编辑网络代理。' : 'You are editing the network proxy.', { exact: false }).count(), 0, 'proxy editing hint clears once the proxy is saved');
      assert.equal(await workspace.locator('.provider-readable-proxy input').inputValue(), 'socks5h://10.0.0.9:1080');
      assert.equal(writes.length, 1, 'proxy save is immediate and does not start login');
      await workspace.getByRole('button', { name: chinese ? '关闭' : 'Close', exact: true }).click();
      await page.getByRole('heading', { name: chinese ? '编辑 reauthorize@example.org' : 'Edit reauthorize@example.org', exact: true }).waitFor();
      await reauthorize.click();
      await start.click();
      await workspace.getByText('FIXTURE-CODE', { exact: true }).waitFor();
      const copyCode = workspace.getByRole('button', { name: chinese ? '复制设备验证码' : 'Copy device code', exact: true });
      await copyCode.focus(); await page.keyboard.press('Space');
      assert.equal(await page.evaluate(() => navigator.clipboard.readText()), 'FIXTURE-CODE');
      await page.evaluate("Object.defineProperty(navigator.clipboard, 'writeText', { configurable: true, value: () => Promise.reject(new Error('fixture-denied')) })");
      await copyCode.click();
      await workspace.getByText(chinese ? '复制失败，请选择验证码后手动复制。' : 'Copy failed. Select the device code and copy it manually.').waitFor();
      assert.equal(await workspace.getByRole('button', { name: chinese ? '检查授权结果' : 'Check authorization', exact: true }).count(), 0);
      for (const width of [390, 1440]) {
        await page.setViewportSize({ width, height: 1000 });
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
        const heights = await workspace.locator('.oauth-login-link-actions .fui-Button, .device-code-actions .fui-Button').evaluateAll(buttons => buttons.map(button => button.getBoundingClientRect().height));
        assert.equal(new Set(heights).size, 1, 'copy code, copy login link, and open login link share button sizing');
      }
      const failed = page.waitForResponse(response => response.url().endsWith('/oauth/codex/poll'));
      await page.clock.fastForward(6_000); await failed;
      await workspace.getByText(chinese ? '暂时无法确认授权状态，系统会稍后自动重试。请勿重复开始登录。' : 'Authorization could not be confirmed yet. The system will retry automatically. Do not start another login.').waitFor();
      assert.equal(pollCount, 1);
      const stored = await page.evaluate(() => sessionStorage.getItem('mtc-codex-device-recovery'));
      assert.ok(stored?.includes('00000000-0000-4000-8000-000000000001'));
      assert.ok(!stored?.includes('synthetic-device-session') && !stored?.includes('FIXTURE-CODE'), 'only a non-secret recovery reference is persisted');
      const pending = page.waitForResponse(response => response.url().endsWith('/oauth/codex/poll'));
      await page.reload(); await pending;
      await workspace.getByText(chinese ? '请在提供商页面完成登录；系统会自动检测结果，成功后返回账号页面，无需手动检查。' : 'Finish signing in on the provider page. The system checks automatically and returns to the account page on success; no manual check is needed.', { exact: true }).first().waitFor();
      await page.clock.fastForward(9_000); assert.equal(pollCount, 2);
      await page.clock.fastForward(1_000);
      await workspace.waitFor({ state: 'detached' });
      await page.getByText(chinese ? '已登录，账号授权已更新。' : 'Signed in. Account authorization updated.', { exact: true }).waitFor();
      await page.locator('.provider-detail-workspace').waitFor();
      assert.equal(await page.evaluate(() => sessionStorage.getItem('mtc-codex-device-recovery')), null);
      assert.equal(writes.filter(write => write.path.endsWith('/start')).length, 1);
      assert.equal(pollCount, 3);
      await page.clock.fastForward(60_000); assert.equal(pollCount, 3, 'successful login stops polling');
      await reauthorize.click();
      await workspace.getByRole('button', { name: chinese ? '关闭' : 'Close', exact: true }).click();
      assert.equal(await reauthorize.evaluate(element => element === document.activeElement), true);
      assert.equal(writes.filter(write => write.path.endsWith('/start')).length, 1, 'reopening and closing never repeats login');
      expiredRecovery = true;
      await page.evaluate(() => sessionStorage.setItem('mtc-codex-device-recovery', JSON.stringify({ session_id: '00000000-0000-4000-8000-000000000001', tenant: 'fixture-a', account_id: 'reauthorization-fixture', expires_at: Date.now() - 1000 })));
      await page.reload();
      await workspace.getByText(chinese ? '本次登录已过期。请返回登录设置后重新开始。' : 'This login expired. Return to login setup to start again.', { exact: true }).waitFor();
      const expiredPolls = pollCount;
      await page.clock.fastForward(60_000);
      assert.equal(pollCount, expiredPolls, 'an expired pending session stops status polling');
      assert.equal(writes.filter(write => write.path.endsWith('/start')).length, 1, 'expiry never starts another OAuth session');
      assert.equal(await page.evaluate(() => sessionStorage.getItem('mtc-codex-device-recovery')), null);
      await page.close();
    }
  } finally { await browser.close(); await server.close(); }
});
