import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

test('service footer preserves shell, distinguishes dependency failures and historical write evidence', { timeout: 60_000 }, async (context) => {
  const executablePath = chromium.executablePath();
  if (!existsSync(executablePath)) {
    assert.notEqual(process.env.MTC_REQUIRE_BROWSER, '1', 'CI requires Chromium');
    context.skip('Chromium unavailable');
    return;
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ executablePath, headless: true });
  try {
    for (const locale of ['en', 'zh-CN']) {
      const page = await browser.newPage();
      await page.addInitScript((value) => { localStorage.setItem('mtc-locale', value); }, locale);
      let status = 503;
      let body: object = { status: 'not_ready', error: { code: 'database_storage_full' } };
      await page.route('**/ui-assets/service-status', async (route) => {
        assert.equal(route.request().headers().authorization, undefined);
        await route.fulfill({ status, contentType: 'application/json', body: JSON.stringify(body) });
      });
      await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/service-availability.html`);
      const footer = page.locator('footer');
      await footer.waitFor();
      await page.getByRole('heading', { name: 'Existing application shell' }).waitFor();
      assert.match(await footer.innerText(), locale === 'en' ? /storage.*full/i : /存储空间已满/);
      assert.match(await footer.innerText(), locale === 'en' ? /do not need to re-enter/ : /无需重新填写/);
      for (const width of [320, 1440]) {
        await page.setViewportSize({ width, height: 900 });
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth), true);
      }
      body = { status: 'not_ready', error: { code: 'database_unavailable' } };
      await footer.getByRole('button').click();
      await page.waitForFunction(() => /cannot be verified|无法确认数据和权限/.test(document.querySelector('footer')?.textContent ?? ''));
      status = 200;
      body = { status: 'ready', checks: { database: 'ok' }, last_database_error: { code: 'database_storage_full', observed_at: 1_792_068_000 } };
      await footer.getByRole('button').click();
      await page.waitForFunction(() => /does not confirm|不确认写入/.test(document.querySelector('footer')?.textContent ?? ''));
      assert.equal(await footer.locator('time').count(), 1);
      body = { status: 'ready', checks: { database: 'ok' }, last_database_error: null };
      await footer.getByRole('button').click();
      await footer.waitFor({ state: 'detached' });
      await page.getByRole('heading', { name: 'Existing application shell' }).waitFor();
      await page.close();
    }
  } finally { await browser.close(); await server.close(); }
});
