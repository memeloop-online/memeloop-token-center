import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

test('quota reset discovery survives missing and failed reads without performing quota operations', { timeout: 60_000 }, async () => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    return test.skip('Chromium required');
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const base = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    const errors: string[] = [];
    const forbiddenRequests: string[] = [];
    page.on('pageerror', (error) => errors.push(error.message));
    await page.route('**/*', (route) => {
      const url = new URL(route.request().url());
      if (url.origin !== base || url.pathname.startsWith('/internal/')) {
        forbiddenRequests.push(url.pathname);
        return route.abort();
      }
      return route.continue();
    });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`${base}/e2e/fixtures/quota-discovery.html`);
    for (const state of ['pending', 'failed', 'snapshot']) {
      const section = page.locator(`[data-state="${state}"]`);
      await section.locator('summary').click();
      const reset = section.getByRole('button', { name: 'Reset upstream quota', exact: true });
      assert.equal(await reset.isEnabled(), state === 'snapshot');
      // Never click reset or issue a quota read, preparation, confirmation or reconciliation.
      if (state === 'pending') assert.match(await section.innerText(), /has not been read/);
      if (state === 'failed') assert.match(await section.innerText(), /could not be read/);
    }
    for (const theme of ['light', 'dark']) {
      await page.setViewportSize({ width: 390, height: 844 });
      await page.evaluate((value) => { document.documentElement.dataset.theme = value; }, theme);
      assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth));
    }
    assert.deepEqual(forbiddenRequests, []);
    assert.deepEqual(errors, []);
  } finally {
    await browser.close();
    await server.close();
  }
});
