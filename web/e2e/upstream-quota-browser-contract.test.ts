import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { mkdir } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

test('static quota and confirmation evidence without clicking quota controls', { timeout: 90_000 }, async () => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    return test.skip('Chromium required');
  }
  const root = fileURLToPath(new URL('..', import.meta.url));
  const server = await createServer({ root, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const base = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    const errors: string[] = [];
    page.on('pageerror', (error) => errors.push(error.message));
    await page.route('**/*', (route) => {
      const url = new URL(route.request().url());
      return url.origin !== base || url.pathname.startsWith('/internal/') ? route.abort() : route.continue();
    });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    const url = `${base}/e2e/fixtures/upstream-ux-static.html`;
    const artifacts = join(root, 'e2e-artifacts', 'upstream-quota');
    await mkdir(artifacts, { recursive: true });
    await page.goto(url);
    await page.getByText('Stale data', { exact: true }).waitFor();
    assert.equal(await page.getByRole('meter').count(), 1, 'unknown usage never renders a zero/full meter');
    assert.equal(await page.getByRole('button', { name: 'Reset upstream quota', exact: true }).isEnabled(), true);
    for (const theme of ['light', 'dark']) {
      await page.evaluate((value) => { document.documentElement.dataset.theme = value; }, theme);
      for (const width of [320, 390, 768, 1024, 1440, 1920, 2560]) {
        await page.setViewportSize({ width, height: 900 });
        assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth));
        await page.screenshot({ path: join(artifacts, `upstream-quota-${theme}-${width}.png`), fullPage: true });
      }
    }
    await page.goto(`${url}?confirm`);
    const dialog = page.getByRole('dialog');
    await dialog.waitFor();
    assert.match(await dialog.innerText(), /Mock Codex.*mock-account.*1 upstream reset credit/s);
    assert.equal(await dialog.getByRole('button', { name: 'Cancel', exact: true }).evaluate((node) => node === document.activeElement), true);
    await page.keyboard.press('Escape');
    await dialog.waitFor({ state: 'hidden' });
    assert.deepEqual(errors, []);
  } finally {
    await browser.close();
    await server.close();
  }
});
