import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer } from './support/isolated-vite-server.js';

test('Antigravity reauthorization preserves the account and consumes identity-mismatched sessions without replay', { timeout: 60_000 }, async () => {
  const server = await createIsolatedFixtureServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen(); const address = server.httpServer!.address(); assert.ok(address && typeof address !== 'string');
  const origin = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 390, height: 1000 }, hasTouch: true });
    await page.clock.setFixedTime(new Date('2026-09-15T00:00:00Z'));
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'zh-CN'));
    let starts = 0; let completes = 0; let reads = 0;
    const errors: string[] = []; page.on('pageerror', error => errors.push(error.message));
    await page.route('**/*', async route => {
      const url = new URL(route.request().url());
      if (url.origin !== origin) return route.abort();
      if (!url.pathname.startsWith('/internal/')) return route.continue();
      if (url.pathname === '/internal/v1/upstreams') {
        assert.equal(route.request().method(), 'GET'); reads++;
        return route.fulfill({ json: [] });
      }
      assert.equal(route.request().method(), 'POST');
      const body = route.request().postDataJSON();
      if (url.pathname.endsWith('/authorization-code/start')) {
        starts++;
        assert.deepEqual(body, { tenant_external_id: 'fixture-a', account_name: 'Original account', provider_driver: 'google-antigravity', upstream_account_id: 'original-account' });
        return route.fulfill({ json: { driver: 'google-antigravity', login_url: 'https://login.example.invalid/authorize', session_token: 'fixture-session', expires_at: Date.now() + 600_000, recovery_expires_at: Date.now() + 86_400_000 } });
      }
      assert.equal(url.pathname, '/internal/v1/oauth/authorization-code/complete'); completes++;
      assert.equal(body.session_token, 'fixture-session');
      if (completes === 1) {
        assert.equal(body.callback_url, 'http://localhost/callback?code=fixture-code&state=fixture-state');
        return route.fulfill({ status: 409, json: { error: { code: 'oauth_identity_mismatch', message: 'must-not-display-secret' } } });
      }
      if (completes === 2) {
        assert.equal(body.callback_url, 'http://localhost/callback?code=fixture-code&state=fixture-state');
        return route.fulfill({ status: 409, json: { error: { message: 'no issued result yet' } } });
      }
      assert.equal(body.callback_url, '', 'recoverable continuation never resends the code');
      return route.fulfill({ status: 200, json: { id: 'original-account', name: 'Original account', credential_generation: 5 } });
    });
    const path = `${origin}/e2e/fixtures/authorization-code.html`;
    for (const mode of ['legacy', 'other']) {
      await page.goto(`${path}?reauthorize=${mode}`);
      await page.getByRole('status').filter({ hasText: '保持账号连接唯一' }).waitFor();
      assert.equal(await page.getByRole('button', { name: '开始登录', exact: true }).count(), 0);
    }
    assert.equal(starts, 0);
    await page.goto(`${path}?reauthorize=eligible`);
    assert.equal(await page.getByLabel('上游名称', { exact: true }).isDisabled(), true);
    assert.equal(await page.getByRole('checkbox', { name: '使用账号网络代理', exact: true }).count(), 0);
    assert.match(await page.locator('body').innerText(), /原来的 Google 身份/);
    const start = page.getByRole('button', { name: '开始登录', exact: true });
    await start.click();
    const callback = page.getByLabel('完整回调地址', { exact: true });
    await callback.fill('http://localhost/callback?code=fixture-code&state=fixture-state');
    await page.getByRole('button', { name: '完成授权', exact: true }).click();
    await page.getByRole('alert').filter({ hasText: '登录身份与原账号不同' }).waitFor();
    assert.equal(await page.getByRole('button', { name: '继续完成重新授权', exact: true }).count(), 0);
    assert.equal(await callback.count(), 0);
    assert.equal(await start.count(), 0, 'a rejected session requires explicit draft reset before another start');
    assert.equal(completes, 1); assert.equal(reads, 0);
    assert.doesNotMatch(await page.locator('body').innerText(), /must-not-display-secret|fixture-code|fixture-session/);
    await page.getByRole('button', { name: '清除登录草稿', exact: true }).click();
    await page.getByRole('dialog').getByRole('button', { name: '确认继续', exact: true }).click();
    await start.click(); await callback.fill('http://localhost/callback?code=fixture-code&state=fixture-state');
    await page.getByRole('button', { name: '完成授权', exact: true }).click();
    await page.getByRole('alert').filter({ hasText: '原账号的重新授权状态待确认' }).waitFor();
    await page.getByRole('button', { name: '继续完成重新授权', exact: true }).click();
    await page.getByRole('status').filter({ hasText: '原账号已重新授权' }).waitFor();
    assert.equal(starts, 2); assert.equal(completes, 3); assert.equal(reads, 1);
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false);
    assert.deepEqual(errors, []);
  } finally { await browser.close(); await server.close(); }
});
