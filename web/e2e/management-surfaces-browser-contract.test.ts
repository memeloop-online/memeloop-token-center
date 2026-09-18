import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer } from './support/isolated-vite-server.js';

test('management surfaces stay flat across themes without removing controls, focus or excluded quota borders', { timeout: 60_000 }, async () => {
  const root = fileURLToPath(new URL('..', import.meta.url));
  const server = await createIsolatedFixtureServer({ root, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen(); const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    const artifacts = `${root}/e2e-artifacts/management-surfaces`; await mkdir(artifacts, { recursive: true });
    for (const view of ['providers', 'routes', 'pricing', 'credentials', 'plugins', 'system-settings']) {
      await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/management-surfaces.html?view=${view}`);
      for (const theme of ['light', 'dark']) for (const width of [390, 1440]) {
        await page.setViewportSize({ width, height: 1000 });
        await page.evaluate(theme => { document.documentElement.dataset.theme = theme; }, theme);
        const frame = await page.getByTestId('outer-surface').evaluate(el => { const style = getComputedStyle(el); return { shadow: style.boxShadow, border: style.borderLeftWidth }; });
        assert.deepEqual(frame, { shadow: 'none', border: '0px' }, `${view}/${theme}/${width} has no redundant outer frame`);
        assert.equal(await page.getByTestId('excluded-quota').evaluate(el => getComputedStyle(el).borderTopWidth), '1px');
        assert.equal(await page.getByTestId('excluded-overview').evaluate(el => getComputedStyle(el).borderTopWidth), '1px');
        const input = page.getByLabel('连接名称', { exact: true }); await input.focus();
        assert.equal(await input.evaluate(el => document.activeElement === el), true);
        assert.notEqual(await input.evaluate(el => getComputedStyle(el).outlineWidth), '0px', 'keyboard focus outline remains');
        assert.notEqual(await input.locator('..').evaluate(el => getComputedStyle(el).borderBottomWidth), '0px', 'Fluent input border remains');
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false);
        if (width === 390 && theme === 'dark') await page.screenshot({ path: `${artifacts}/${view}-${theme}-${width}.png`, fullPage: true });
      }
    }
  } finally { await browser.close(); await server.close(); }
});
