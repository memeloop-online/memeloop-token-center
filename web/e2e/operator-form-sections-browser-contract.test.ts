import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

test('advanced validation remains discoverable without losing field values or mobile readability', { timeout: 30_000 }, async (context) => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    context.skip('Chromium required'); return;
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 390, height: 844 } });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/operator-form-sections.html`);
    const timeout = page.getByRole('spinbutton', { name: 'Timeout seconds' });
    const advanced = page.locator('.operator-form-advanced');
    await page.getByLabel('Connection name').waitFor();
    assert.equal(await advanced.getAttribute('open'), null);
    await page.getByRole('button', { name: 'Save fixture' }).click();
    await timeout.waitFor({ state: 'visible' });
    await timeout.fill('30');
    await advanced.locator('summary').focus();
    await page.keyboard.press('Enter');
    await timeout.waitFor({ state: 'hidden' });
    await page.keyboard.press('Enter');
    assert.equal(await timeout.inputValue(), '30');
    for (const theme of ['light', 'dark']) {
      await page.evaluate(value => { document.documentElement.dataset.theme = value; }, theme);
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
      assert.equal(await page.getByLabel('Connection name').evaluate(element => getComputedStyle(element).fontSize), '16px');
    }
  } finally { await browser.close(); await server.close(); }
});
