import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer } from './support/isolated-vite-server.js';

test('Kimi device login is explicit, respects poll intervals and expiry, and preserves account identity and scope', { timeout: 60_000 }, async () => {
  const server = await createIsolatedFixtureServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen(); const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const url = `http://127.0.0.1:${address.port}/e2e/fixtures/authorization-code.html?full-page&scope-controls`;
  const browser = await chromium.launch({ headless: true });
  const account = { id: 'original-kimi', tenant_external_id: 'fixture-a', driver: 'kimi-oauth', name: 'My Kimi', auth_kind: 'oauth', connection_method: 'oauth', status: 'active', credential_generation: 3, updated_at: 4, route_count: 2, config: {}, can_reauthorize: true, has_proxy: false };
  const provider = { id: 'kimi-oauth', display_name: 'Kimi', source: 'builtin', protocols: ['anthropic'], modalities: ['text'], config_schema: { type: 'object' }, credential_schema: { type: 'object', properties: { type: { const: 'oauth' } } }, oauth_adapter: { flow_kind: 'kimi_device' } };
  const initialTime = new Date('2026-09-14T12:00:00Z');
  async function open(reauthorize = false) {
    const page = await browser.newPage({ viewport: { width: 390, height: 1000 } });
    page.setDefaultTimeout(5_000);
    await page.clock.install({ time: initialTime });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'zh-CN'));
    const writes: { path: string; body: Record<string, unknown> }[] = [];
    const state = { saved: false, failList: false, polls: 0, holdStart: false };
    let releaseStart: (() => void) | undefined;
    await page.route('**/internal/v1/**', async route => {
      const request = route.request(), path = new URL(request.url()).pathname;
      if (request.method() !== 'GET') {
        assert.equal(request.method(), 'POST');
        assert.ok(['/internal/v1/oauth/kimi/start', '/internal/v1/oauth/kimi/poll'].includes(path), 'no approval, refresh, reset or other write is allowed');
        writes.push({ path, body: request.postDataJSON() });
        if (path.endsWith('/start')) {
          if (state.holdStart) await new Promise<void>(resolve => { releaseStart = resolve; });
          return route.fulfill({ json: { driver: 'kimi-oauth', verification_url: 'https://www.kimi.com/device?user_code=MOCK-KIMI', user_code: 'MOCK-KIMI', session_token: 'mock-kimi-session', expires_at: initialTime.getTime() + 120_000, poll_after_seconds: 5, security_notice: 'only_continue_if_you_started_this_login' } });
        }
        state.polls += 1;
        assert.deepEqual(request.postDataJSON(), { session_token: 'mock-kimi-session' });
        if (state.polls === 1) return route.fulfill({ status: 202, json: { status: 'pending', retry_after_seconds: 10 } });
        state.saved = true;
        return route.fulfill({ status: reauthorize ? 200 : 201, json: account });
      }
      if (path === '/internal/v1/provider-types') return route.fulfill({ json: [provider] });
      if (path === '/internal/v1/upstreams') return route.fulfill(state.saved && state.failList ? { status: 503, json: { error: { message: 'mock list read unavailable' } } } : { json: reauthorize || state.saved ? [account] : [] });
      return route.fulfill({ json: [] });
    });
    await page.goto(url);
    if (reauthorize) {
      await page.locator('[data-inline-edit-trigger="original-kimi"]').click();
      await page.getByRole('button', { name: '重新授权', exact: true }).click();
    } else {
      await page.locator('.create-journey [data-workspace-toggle]').click();
      await page.getByRole('button', { name: '账户授权', exact: true }).click();
    }
    return { page, writes, state, releaseStart: () => { assert.ok(releaseStart); releaseStart(); } };
  }
  try {
    for (const reauthorize of [false, true]) {
      const { page, writes, state } = await open(reauthorize);
      assert.equal(await page.getByRole('button', { name: '开始登录', exact: true }).isEnabled(), true, 'Kimi permits direct login without a proxy');
      if (reauthorize) assert.equal(await page.getByRole('checkbox', { name: '使用账号网络代理' }).count(), 0);
      await page.getByRole('button', { name: '开始登录', exact: true }).click();
      await page.getByText('MOCK-KIMI', { exact: true }).waitFor();
      assert.deepEqual(writes[0].body, { tenant_external_id: 'fixture-a', account_name: reauthorize ? 'My Kimi' : 'Kimi', ...(reauthorize ? { upstream_account_id: 'original-kimi' } : {}) });
      assert.equal(await page.getByRole('link', { name: '打开授权页', exact: true }).getAttribute('href'), 'https://www.kimi.com/device?user_code=MOCK-KIMI');
      assert.equal(await page.getByText(/才在 OpenAI 页面继续/).count(), 0);
      assert.equal(await page.getByRole('button', { name: /秒后可检查/ }).isDisabled(), true);
      await page.clock.fastForward(6_000);
      assert.equal(state.polls, 0, 'time passing never polls or approves automatically');
      await page.getByRole('button', { name: '检查授权结果', exact: true }).click();
      await page.getByRole('button', { name: '10 秒后可检查', exact: true }).waitFor();
      assert.equal(state.polls, 1);
      await page.clock.fastForward(9_000);
      assert.equal(await page.getByRole('button', { name: /秒后可检查/ }).isDisabled(), true, 'the server slow-down interval is honored');
      await page.clock.fastForward(1_000);
      state.failList = true;
      await page.getByRole('button', { name: '检查授权结果', exact: true }).click();
      await page.getByText('账号已保存，但列表暂时无法读取。请重试读取，无需重新登录。', { exact: true }).waitFor();
      assert.equal(state.polls, 2); assert.equal(writes.filter(write => write.path.endsWith('/start')).length, 1);
      state.failList = false;
      await page.getByRole('button', { name: '重新读取账号列表', exact: true }).click();
      await page.getByRole('button', { name: '重新读取账号列表', exact: true }).waitFor({ state: 'hidden' });
      assert.equal(state.polls, 2, 'list retry never exchanges credentials again');
      await page.close();
    }
    const expired = await open();
    await expired.page.getByRole('button', { name: '开始登录', exact: true }).click();
    await expired.page.getByText('MOCK-KIMI', { exact: true }).waitFor();
    const screenshots = fileURLToPath(new URL('../e2e-artifacts/upstream-availability', import.meta.url)); await mkdir(screenshots, { recursive: true });
    for (const width of [390, 1440]) {
      await expired.page.setViewportSize({ width, height: 1000 });
      assert.equal(await expired.page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
      await expired.page.screenshot({ path: `${screenshots}/kimi-device-${width}.png` });
    }
    await expired.page.clock.fastForward(121_000);
    await expired.page.getByText('本次登录已过期。请返回登录设置后重新开始。', { exact: true }).waitFor();
    assert.equal(await expired.page.getByText('MOCK-KIMI', { exact: true }).count(), 0);
    assert.equal(await expired.page.getByRole('link', { name: '打开授权页', exact: true }).count(), 0);
    await expired.page.getByRole('button', { name: '返回登录设置', exact: true }).click();
    assert.equal(expired.writes.length, 1, 'returning to settings does not automatically start another login');
    await expired.page.close();
    for (const scopeButton of ['Switch tenant', 'Switch credential']) {
      const scoped = await open(); scoped.state.holdStart = true;
      const started = scoped.page.waitForRequest(request => request.url().endsWith('/oauth/kimi/start'));
      await scoped.page.getByRole('button', { name: '开始登录', exact: true }).click(); await started;
      await scoped.page.getByRole('button', { name: scopeButton, exact: true }).click();
      const completed = scoped.page.waitForResponse(response => response.url().endsWith('/oauth/kimi/start'));
      scoped.releaseStart(); await completed;
      await scoped.page.locator('.create-journey [data-workspace-toggle]').click();
      await scoped.page.getByRole('button', { name: '账户授权', exact: true }).click();
      assert.equal(await scoped.page.getByText('MOCK-KIMI', { exact: true }).count(), 0, 'late previous-scope login cannot restore a session');
      assert.equal(scoped.writes.length, 1);
      await scoped.page.close();
    }
  } finally { await browser.close(); await server.close(); }
});
