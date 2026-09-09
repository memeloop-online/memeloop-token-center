import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

test('filters are non-modal themed popovers and model selection is searchable by provider/account with keyboard dismissal', { timeout: 30_000 }, async () => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required for model picker interaction contracts');
    return test.skip('Chromium is not installed');
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    const writes: string[] = [];
    await page.exposeFunction('recordModelPickerWrite', (path: string) => writes.push(path));
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/model-picker.html`);
    for (const theme of ['light', 'dark']) {
      await page.evaluate((value) => { document.documentElement.dataset.theme = value; }, theme);
      for (const width of [320, 390, 768, 1024, 1440, 1920, 2560]) {
        await page.setViewportSize({ width, height: 900 });
        const trigger = page.locator('.typed-filter-actions button').first();
        await trigger.click();
        const filter = page.locator('.typed-filter-dialog');
        await filter.waitFor({ state: 'visible' });
        assert.equal(await filter.getAttribute('aria-modal'), 'false');
        const bounds = await filter.boundingBox();
        assert.ok(bounds && bounds.x >= 0 && bounds.x + bounds.width <= width);
        const color = await filter.evaluate((element) => getComputedStyle(element).backgroundColor);
        assert.equal(color, theme === 'light' ? 'rgb(255, 255, 255)' : 'rgb(13, 28, 32)');
        await page.keyboard.press('Escape');
        await filter.waitFor({ state: 'hidden' });
        await trigger.click();
        await page.locator('[data-outside]').click();
        await filter.waitFor({ state: 'hidden' });
      }
    }
    assert.deepEqual(writes, [], 'opening, closing, Escape and outside clicks must not persist presets or invoke the assistant');
    await page.setViewportSize({ width: 1440, height: 900 });
    await page.locator('.typed-filter-actions button').first().click();
    const filter = page.locator('.typed-filter-dialog');
    await filter.getByRole('button', { name: 'Add condition', exact: true }).click();
    await filter.locator('.typed-filter-row').getByLabel('Field', { exact: true }).selectOption('model');
    await filter.locator('.model-picker-trigger').click();
    const catalog = filter.locator('.shared-model-popover');
    const search = catalog.getByRole('combobox');
    await search.fill('provider-b');
    await catalog.getByRole('option').first().waitFor();
    assert.equal(await catalog.getByRole('group', { name: 'provider-b', exact: true }).count(), 1);
    assert.equal(await catalog.getByRole('group', { name: 'Production account', exact: true }).count(), 1);
    assert.equal(await catalog.getByRole('option').count(), 1);
    await search.press('ArrowDown');
    await search.press('Enter');
    await filter.getByRole('button', { name: 'Apply filters', exact: true }).click();
    assert.equal(await page.locator('[data-filter-model]').textContent(), 'production-model');
    await page.locator('.system-settings .model-picker-trigger').click();
    const settingsCatalog = page.locator('.system-settings .shared-model-popover');
    await settingsCatalog.getByRole('combobox').fill('Research account');
    await settingsCatalog.getByRole('option').first().click();
    assert.match(await page.locator('.system-settings .model-picker-trigger').textContent() ?? '', /research-model/);
  } finally {
    await browser.close();
    await server.close();
  }
});
