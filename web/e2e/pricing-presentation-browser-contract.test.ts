import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

test('effective limits and imported price provenance stay truthful and accessible', { timeout: 30_000 }, async context => {
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
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/pricing-presentation.html`);
    const unlimited = page.getByRole('region', { name: 'Unlimited credential' });
    await unlimited.waitFor();
    assert.doesNotMatch(await unlimited.innerText(), /4,294|9,007|1,234/);
    assert.match(await unlimited.innerText(), /RPM Unlimited/);
    const prepaid = await page.getByRole('region', { name: 'Prepaid credential' }).innerText();
    assert.match(prepaid, /4,294,967,295/);
    assert.match(prepaid, /9,007,199,254,740,991/);
    assert.match(prepaid, /Not set/);
    assert.doesNotMatch(prepaid, /Unlimited/);
    assert.doesNotMatch(await page.locator('body').innerText(), /cpamp:|copied:/);
    assert.equal(await page.getByText('models.dev', { exact: true }).isVisible(), true);
    const details = page.locator('.price-provenance').first();
    await details.locator('summary').focus();
    await page.keyboard.press('Enter');
    assert.match(await details.innerText(), /cpamp:import-run/);
    for (const width of [390, 1440]) {
      await page.setViewportSize({ width, height: 900 });
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
    }
    await details.locator('summary').focus();
    await page.keyboard.press('Enter');
    assert.doesNotMatch(await page.locator('body').innerText(), /cpamp:/);
  } finally { await browser.close(); await server.close(); }
});
