import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

test('multi-select escapes clipping and supports keyboard selection, dismissal and retry', { timeout: 30_000 }, async (context) => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required for the multi-select contract');
    context.skip('Chromium is not installed'); return;
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 390, height: 844 } });
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/multi-combobox.html`);
    const input = page.getByRole('combobox', { name: 'Workspaces' });
    await input.fill('Workspace 2');
    await input.press('ArrowDown');
    await input.press('Enter');
    assert.equal(await page.getByLabel('Selected count').innerText(), '1');
    assert.equal(await page.getByLabel('Search query').innerText(), 'empty');
    await input.press('Escape');
    await input.press('Enter');
    assert.equal(await page.getByLabel('Selected count').innerText(), '1', 'closed Enter must not silently select another resource');
    await input.press('ArrowDown');
    const menu = page.locator('.multi-combobox-popover');
    assert.equal(await menu.evaluate(element => element.matches(':popover-open')), true);
    const bounds = await menu.boundingBox();
    assert.ok(bounds && bounds.x >= 8 && bounds.x + bounds.width <= 382);
    const last = page.getByRole('option').last();
    await last.click();
    assert.equal(await page.getByLabel('Selected count').innerText(), '2', 'option outside the clipping container remains clickable');
    await input.press('Tab');
    assert.equal(await page.getByRole('button', { name: 'Continue', exact: true }).evaluate(element => element === document.activeElement), true);
    await page.getByRole('button', { name: 'Simulate unavailable search' }).click();
    await input.focus();
    await input.press('Tab');
    const retry = page.getByRole('button', { name: 'Retry search' });
    assert.equal(await retry.evaluate(element => element === document.activeElement), true);
    await retry.press('Enter');
    await page.getByRole('alert').waitFor({ state: 'detached' });
    for (const theme of ['dark', 'light']) {
      await page.evaluate(value => { document.documentElement.dataset.theme = value; }, theme);
      await input.focus();
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
    }
  } finally { await browser.close(); await server.close(); }
});
