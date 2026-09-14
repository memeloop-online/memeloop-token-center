import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer } from './support/isolated-vite-server.js';

test('plugin OAuth uses default client, full callback once, safe errors and isolated scope', { timeout: 60_000 }, async () => {
  const root = fileURLToPath(new URL('..', import.meta.url));
  const server = await createIsolatedFixtureServer({ root, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 390, height: 900 } });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'zh-CN'));
    let starts = 0; let completes = 0;
    await page.route('**/internal/v1/oauth/**', async route => {
      const body = route.request().postDataJSON();
      if (route.request().url().endsWith('/start')) {
        starts++; assert.equal(body.provider_driver, 'fixture-plugin'); assert.equal('client' in body, false);
        if (starts === 1) return route.fulfill({ status: 409, json: { error: { message: 'default OAuth client configuration is not provisioned' } } });
        return route.fulfill({ json: { driver: 'fixture-plugin', login_url: 'https://login.example.invalid/authorize', session_token: 'fixture-session-secret', expires_at: Date.now() + 600_000 } });
      }
      assert.ok(route.request().url().endsWith('/complete')); completes++;
      assert.equal(body.callback_url, 'http://localhost:8080/callback?code=fixture-code&state=fixture-state');
      return route.fulfill({ status: 502, json: { error: { message: 'must-not-display-sensitive-token' } } });
    });
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/authorization-code.html`);
    const start = page.getByRole('button', { name: '开始登录', exact: true });
    await start.click(); await page.getByRole('alert').waitFor();
    assert.match(await page.getByRole('alert').innerText(), /MTC_PROVIDER_OAUTH_CLIENT_DEFAULTS_JSON/);
    await start.click(); await page.getByLabel('完整回调地址', { exact: true }).waitFor();
    await page.getByLabel('完整回调地址', { exact: true }).fill('http://localhost:8080/callback?code=fixture-code&state=fixture-state');
    await page.getByRole('button', { name: '完成授权', exact: true }).click();
    await page.getByRole('alert').waitFor();
    assert.equal(completes, 1); assert.doesNotMatch(await page.locator('body').innerText(), /must-not-display|fixture-code|fixture-session-secret/);
    assert.equal(await page.getByRole('button', { name: '完成授权', exact: true }).count(), 0);
    const artifacts = `${root}/e2e-artifacts/oauth-authorization-code`; await mkdir(artifacts, { recursive: true });
    await page.screenshot({ path: `${artifacts}/mobile-safe-error.png`, fullPage: true });
    await page.getByRole('button', { name: 'Switch scope' }).click();
    await start.waitFor(); assert.match(await page.locator('body').innerText(), /fixture-b/);
    assert.equal(await page.getByLabel('完整回调地址', { exact: true }).count(), 0);
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > document.documentElement.clientWidth), false);
  } finally { await browser.close(); await server.close(); }
});
