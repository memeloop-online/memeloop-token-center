import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { mkdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer } from './support/isolated-vite-server.js';

test('live metrics share real-data backgrounds and neutral rates across themes and widths', { timeout: 45_000 }, async context => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    context.skip('Chromium required'); return;
  }
  const server = await createIsolatedFixtureServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch();
  try {
    const page = await browser.newPage();
    const errors: string[] = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/shared-request-metrics.html`);
    const cards = page.locator('.request-traffic-metrics .analytics-metric');
    await cards.first().waitFor();
    assert.equal(await cards.count(), 6);
    assert.deepEqual(await cards.locator('.metric-value').allTextContents(), ['3', '1', '1', '1', '50%', '25 s']);
    const rate = cards.filter({ hasText: 'Finished request success rate' });
    assert.equal(await rate.locator('.analytics-metric-ratio').getAttribute('data-ratio'), '0.5');
    assert.equal(await rate.evaluate(node => node.classList.contains('positive')), false);
    assert.ok(await cards.locator('.analytics-metric-trend').count() >= 4);
    await cards.first().focus();
    await page.keyboard.press('End');
    assert.match(await cards.first().getAttribute('aria-valuetext') ?? '', /Requests: 1/);
    const descriptions = await cards.first().evaluate(node => (node.getAttribute('aria-describedby') ?? '').split(/\s+/).map(id => document.getElementById(id)?.textContent ?? ''));
    assert.ok(descriptions.includes('3'), 'interactive bucket inspection must retain the aggregate in its accessible description');
    const average = cards.filter({ hasText: 'Average latency' });
    assert.equal(await average.count(), 1);
    const averageDescription = await average.evaluate(node => (node.getAttribute('aria-describedby') ?? '').split(/\s+/).map(id => document.getElementById(id)?.textContent ?? '').join(' '));
    assert.match(averageDescription, /25 s/);
    await page.keyboard.press('Escape');
    const artifacts = fileURLToPath(new URL('../e2e-artifacts/ui-system/', import.meta.url));
    await mkdir(artifacts, { recursive: true });
    for (const theme of ['light', 'dark']) {
      await page.evaluate(value => { document.documentElement.dataset.theme = value; }, theme);
      for (const width of [390, 1440]) {
        await page.setViewportSize({ width, height: 1000 });
        await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => resolve(null))));
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false);
        assert.notEqual(await cards.first().evaluate(node => getComputedStyle(node).backgroundColor), 'rgba(0, 0, 0, 0)');
        await page.screenshot({ path: `${artifacts}/shared-request-metrics-${theme}-${width}.png` });
      }
    }
    assert.deepEqual(errors, []);
  } finally { await browser.close(); await server.close(); }
});
