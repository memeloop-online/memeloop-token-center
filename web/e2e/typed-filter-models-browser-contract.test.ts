import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

test('request filters find older astra routes, accept historical names, and preview saved conditions on mobile', { timeout: 60_000 }, async (t) => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required for filter model contracts');
    t.skip('Chromium is not installed'); return;
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    for (const locale of ['en', 'zh-CN']) {
      const page = await browser.newPage({ viewport: { width: 320, height: 900 } });
      page.setDefaultTimeout(8_000);
      const errors: string[] = [];
      page.on('pageerror', (error) => errors.push(error.message));
      await page.addInitScript((value) => localStorage.setItem('mtc-locale', value), locale);
      const url = `http://127.0.0.1:${address.port}/e2e/fixtures/typed-filter-models.html`;
      await page.goto(url);
      const open = () => page.locator('.typed-filter-actions button').first().click();
      await open();
      const presets = page.locator('.typed-filter-preset-list button');
      await presets.last().waitFor();
      assert.match(await presets.last().innerText(), /gpt-6-astra/);
      assert.ok((await presets.last().innerText()).includes(locale === 'en' ? 'Failed' : '失败'));
      assert.ok((await presets.first().getAttribute('title'))?.includes('gpt-6-astra'));
      const preview = await presets.last().getAttribute('title');
      assert.equal(await presets.last().getAttribute('aria-label'), preview);
      assert.equal(await page.locator('.typed-filter-dialog').evaluate((element) => element.scrollWidth > element.clientWidth), false);
      const model = page.locator('.typed-filter-model-picker input[role=combobox]');
      await model.fill('slow');
      await page.waitForFunction(() => JSON.parse(document.body.dataset.catalogRequests ?? '[]').includes('slow'));
      await model.fill('astra');
      const option = page.getByRole('option').filter({ has: page.getByText('gpt-6-astra', { exact: true }) });
      await option.waitFor();
      await page.waitForTimeout(750);
      assert.equal(await model.inputValue(), 'astra', 'late search must not overwrite the current input');
      assert.equal(await page.locator('.shared-model-popover').getByRole('option', { name: 'slow-historical-model', exact: true }).count(), 0);
      assert.equal(await option.isEnabled(), true, 'historical model selection must not require a usable account');
      assert.equal(await option.count(), 1, 'one name represents all accounts');
      assert.equal(await page.locator('.shared-model-popover').getByRole('group').count(), 0);
      await model.press('ArrowDown');
      await model.press('Enter');
      assert.equal(await model.inputValue(), 'gpt-6-astra');
      await page.setViewportSize({ width: 1280, height: 900 });
      const controls = await page.locator('.typed-filter-row').first().locator('select, .shared-model-picker > input').evaluateAll((elements) => elements.map((element) => { const rect = element.getBoundingClientRect(); return { top: rect.top, height: rect.height }; }));
      assert.equal(controls.length, 3);
      assert.ok(Math.max(...controls.map((c) => c.top)) - Math.min(...controls.map((c) => c.top)) <= 2, 'field, operator and model input share the same baseline');
      for (const width of [320, 375]) {
        await page.setViewportSize({ width, height: 900 });
        assert.equal(await page.locator('.typed-filter-dialog').evaluate((element) => element.scrollWidth > element.clientWidth), false);
      }
      const apply = page.getByRole('button', { name: locale === 'en' ? 'Apply filters' : '应用筛选', exact: true });
      await apply.click();
      await page.locator('.typed-filter-dialog').waitFor({ state: 'detached' });
      assert.equal(JSON.parse(await page.locator('#applied-filter').innerText()).conditions[0].value.value, 'gpt-6-astra');
      await open();
      await model.fill('historical-model-outside-catalog');
      await model.press('Escape');
      await apply.click();
      await page.locator('.typed-filter-dialog').waitFor({ state: 'detached' });
      assert.equal(JSON.parse(await page.locator('#applied-filter').innerText()).conditions[0].value.value, 'historical-model-outside-catalog');
      await open();
      await page.locator('.typed-filter-row').first().getByRole('combobox', { name: locale === 'en' ? 'Operator' : '操作符', exact: true }).selectOption('contains');
      await model.fill('astra');
      await model.press('Escape');
      await apply.click();
      await page.locator('.typed-filter-dialog').waitFor({ state: 'detached' });
      const contains = JSON.parse(await page.locator('#applied-filter').innerText()).conditions[0];
      assert.equal(contains.operator, 'contains'); assert.equal(contains.value.value, 'astra');
      await page.goto(`${url}?catalogError=1`);
      await open();
      await model.fill('gpt-6-astra');
      await page.locator('.shared-model-popover [role="alert"]').waitFor();
      assert.equal(await model.inputValue(), 'gpt-6-astra');
      await model.press('Escape');
      await apply.click();
      await page.locator('.typed-filter-dialog').waitFor({ state: 'detached' });
      assert.equal(JSON.parse(await page.locator('#applied-filter').innerText()).conditions[0].value.value, 'gpt-6-astra');
      assert.deepEqual(errors, []);
      await page.close();
    }
  } finally { await browser.close(); await server.close(); }
});
