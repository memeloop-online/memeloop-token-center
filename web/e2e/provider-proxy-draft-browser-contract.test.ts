import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer } from './support/isolated-vite-server.js';

test('independent proxy save updates concurrency metadata without dropping the provider draft', { timeout: 60_000 }, async () => {
  const server = await createIsolatedFixtureServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const origin = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    const errors: string[] = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.route('**/*', route => {
      const url = new URL(route.request().url());
      return url.origin === origin && !url.pathname.startsWith('/internal/') ? route.continue() : route.abort();
    });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'zh-CN'));
    // All saves below are handled by an explicit in-memory fixture. No server
    // mutation, real proxy connection, OAuth, quota action or model request runs.
    await page.goto(`${origin}/e2e/fixtures/form-journey.html?workflows&proxy-workflow`);
    await page.getByText('账号设置与授权操作', { exact: true }).click();
    await page.getByRole('button', { name: '编辑', exact: true }).click();
    const workspace = page.locator('.provider-edit-workspace');
    const name = workspace.getByLabel('上游名称', { exact: false });
    await name.fill('保留名称草稿');
    await page.getByRole('button', { name: '配置网络代理', exact: true }).click();
    const save = workspace.locator('.rjsf > button[type="submit"]');
    assert.equal(await save.isEnabled(), false);
    await page.locator('.upstream-proxy-editor input').fill('socks5h://10.0.0.10:1080');
    await page.getByRole('button', { name: '保存网络代理', exact: true }).click();
    await page.waitForFunction(() => !document.querySelector<HTMLButtonElement>('.provider-edit-workspace .rjsf > button[type="submit"]')?.disabled);
    assert.equal(await name.inputValue(), '保留名称草稿', 'proxy refresh preserves the independent name draft');
    assert.equal(await page.evaluate(() => window.formJourneyWrites), 1);
    await save.click();
    await page.locator('.provider-list').getByText('保留名称草稿', { exact: true }).waitFor();
    assert.equal(await page.evaluate(() => window.formJourneyWrites), 2, 'provider save succeeds against the new revision without retries');
    assert.deepEqual(errors, []);
  } finally { await browser.close(); await server.close(); }
});
