import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { mkdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer } from './support/isolated-vite-server.js';

test('live metrics retain only actionable aggregate cards across themes and widths', { timeout: 45_000 }, async context => {
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
    assert.equal(await cards.count(), 4);
    assert.deepEqual(await cards.first().locator('.metric-label').evaluate(node => { const style = getComputedStyle(node); return { transform: style.textTransform, size: style.fontSize }; }), { transform: 'none', size: '13px' });
    assert.deepEqual(await cards.locator('.metric-label').allTextContents(), ['Total tokens', 'Local settlement ⓘ', 'Cache rate', 'Average latency']);
    assert.deepEqual(await cards.locator('.metric-value').allTextContents(), ['300', '€0.00$1.50', '40%', '25 s']);
    const settlement = cards.filter({ hasText: 'Local settlement' });
    assert.deepEqual(await settlement.locator('.usage-cost-lines > span').allTextContents(), ['€0.00', '$1.50'], 'local settlement keeps each recorded currency on its own line and displays the not-observed failure at the zero policy amount');
    assert.equal(await settlement.locator('.analytics-metric-trend, .analytics-metric-ratio').count(), 0, 'mixed currencies must not fabricate a settlement trend or ratio');
    const cacheRate = cards.filter({ hasText: 'Cache rate' });
    assert.equal(await cacheRate.locator('.analytics-metric-ratio').getAttribute('data-ratio'), '0.4');
    assert.equal(await cards.locator('.analytics-metric-trend').count(), 2, 'only total tokens and average latency have enough adjacent loaded values for a real trend');
    await cards.first().focus();
    await page.keyboard.press('End');
    assert.match(await cards.first().getAttribute('aria-valuetext') ?? '', /Total tokens: 0$/, 'the last bucket holds only the not-observed failure, which contributes no actual tokens');
    const descriptions = await cards.first().evaluate(node => (node.getAttribute('aria-describedby') ?? '').split(/\s+/).map(id => document.getElementById(id)?.textContent ?? ''));
    assert.ok(descriptions.includes('300'), 'interactive bucket inspection must retain the aggregate in its accessible description');
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
