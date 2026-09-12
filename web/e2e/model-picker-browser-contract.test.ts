import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

test('filters are non-modal themed popovers and model selection is searchable by provider/account with keyboard dismissal', { timeout: 30_000 }, async (context) => {
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
    page.setDefaultTimeout(5_000);
    const pageErrors: string[] = [];
    page.on('pageerror', (error) => pageErrors.push(error.message));
    const writes: string[] = [];
    await page.exposeFunction('recordModelPickerWrite', (path: string) => writes.push(path));
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/model-picker.html`);
    for (const theme of ['light', 'dark']) {
      await page.evaluate((value) => { document.documentElement.dataset.theme = value; }, theme);
      for (const width of [320, 390, 768, 1024, 1440, 1920, 2560]) {
        context.diagnostic(`checking ${theme} ${width}px dismissal and theme`);
        await page.setViewportSize({ width, height: 900 });
        const trigger = page.locator('.typed-filter-actions button').first();
        await trigger.click();
        const filter = page.locator('.typed-filter-dialog');
        await filter.waitFor({ state: 'visible' });
        assert.equal(await filter.getAttribute('aria-modal'), null);
        const bounds = await filter.boundingBox();
        assert.ok(bounds && bounds.x >= 0 && bounds.x + bounds.width <= width);
        const color = await filter.evaluate((element) => getComputedStyle(element).backgroundColor);
        assert.equal(color, theme === 'light' ? 'rgb(255, 255, 255)' : 'rgb(13, 28, 32)');
        await page.keyboard.press('Escape');
        await filter.waitFor({ state: 'hidden' });
        assert.equal(await page.evaluate(() => document.activeElement === document.querySelector('.typed-filter-actions button')), true, 'Escape restores focus to the non-modal popover trigger');
        await trigger.click();
        await page.locator('[data-outside]').click();
        await filter.waitFor({ state: 'hidden' });
      }
    }
    assert.deepEqual(writes, [], 'opening, closing, Escape and outside clicks must not persist presets or invoke the assistant');
    context.diagnostic('checking nested model picker keyboard selection');
    await page.setViewportSize({ width: 1440, height: 900 });
    await page.locator('.typed-filter-actions button').first().click();
    const filter = page.locator('.typed-filter-dialog');
    await filter.getByRole('button', { name: 'Add condition', exact: true }).click();
    const row = filter.locator('.typed-filter-row');
    await row.waitFor({ state: 'visible' });
    assert.equal(await row.getByLabel('Operator', { exact: true }).count(), 1, 'operator label must not include the select option list');
    await row.getByLabel('Field', { exact: true }).selectOption('model');
    await filter.locator('.model-picker-trigger').click();
    const catalog = filter.locator('.shared-model-popover');
    const search = catalog.getByRole('combobox');
    await search.fill('provider-b');
    await catalog.getByRole('option').first().waitFor();
    assert.equal(await catalog.getByRole('group', { name: 'Production pool', exact: true }).count(), 1);
    assert.equal(await catalog.getByRole('group', { name: 'provider-b', exact: true }).count(), 1);
    assert.equal(await catalog.getByRole('group', { name: 'Production account', exact: true }).count(), 1);
    assert.equal(await catalog.getByRole('option').count(), 1);
    assert.match(await catalog.getByRole('option').first().textContent() ?? '', /Health check not run/);
    await search.press('ArrowDown');
    await search.press('Enter');
    assert.equal(await filter.isVisible(), true, 'closing the nested model picker must not close the filter editor');
    await filter.getByRole('button', { name: 'Apply filters', exact: true }).click();
    assert.equal(await page.locator('[data-filter-model]').textContent(), 'production-model');
    context.diagnostic('checking settings provider/account autocomplete');
    assert.match(await page.locator('.system-settings').getByRole('alert').textContent() ?? '', /configured route .friendly-custom-chat. lacks verifiable text-generation capability evidence/i);
    assert.doesNotMatch(await page.locator('.system-settings .model-picker-trigger').textContent() ?? '', /route-custom/, 'an unverified stored route must not leak a raw route ID into the picker');
    await page.locator('.system-settings .model-picker-trigger').click();
    const settingsCatalog = page.locator('.system-settings .shared-model-popover');
    await settingsCatalog.getByRole('combobox').fill('only');
    assert.equal(await settingsCatalog.getByRole('option').count(), 0, 'non-conversational routes never appear in assistant suggestions');
    await settingsCatalog.getByRole('combobox').fill('omni-moderation-latest');
    assert.equal(await settingsCatalog.getByRole('option').count(), 0, 'known moderation models remain excluded without relying on a name pattern');
    await settingsCatalog.getByRole('combobox').fill('image-analysis-assistant');
    assert.equal(await settingsCatalog.getByRole('option').count(), 1, 'a catalog-proven text alias is not rejected because its name contains image');
    await settingsCatalog.getByRole('combobox').fill('friendly-custom-chat');
    assert.equal(await settingsCatalog.getByRole('option').count(), 0, 'an unobserved custom name is not treated as capability evidence');
    await settingsCatalog.getByRole('combobox').fill('Research account');
    await settingsCatalog.getByRole('option', { name: /research-model/i }).click();
    assert.match(await page.locator('.system-settings .model-picker-trigger').textContent() ?? '', /research-model/);
    await page.locator('.system-settings .model-picker-trigger').click();
    await settingsCatalog.getByRole('combobox').fill('Retired account');
    const unavailable = settingsCatalog.getByRole('option', { name: /retired-model/i });
    assert.equal(await unavailable.getAttribute('aria-disabled'), 'true');
    assert.equal(await unavailable.isDisabled(), true);
    await settingsCatalog.getByRole('combobox').fill('model');
    assert.equal(await settingsCatalog.getByRole('option').first().isDisabled(), true, 'the first visual result exercises disabled-result keyboard handling');
    await settingsCatalog.getByRole('combobox').press('Enter');
    await settingsCatalog.waitFor({ state: 'hidden' });
    assert.match(await page.locator('.system-settings .model-picker-trigger').textContent() ?? '', /production-model/, 'Enter skips an unavailable first result and chooses the first available route');
    assert.match(await page.locator('.system-settings').getByRole('status').last().textContent() ?? '', /5 routes are omitted.+custom model name is not capability evidence/i);
    assert.deepEqual(pageErrors, [], 'model picker interactions must not produce page errors');
  } finally {
    await browser.close();
    await server.close();
  }
});
