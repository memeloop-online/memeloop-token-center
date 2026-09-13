import assert from 'node:assert/strict';
import { existsSync, mkdirSync } from 'node:fs';
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
    const advanced = page.locator('.operator-form-advanced').filter({ hasText: 'Advanced network' });
    await page.getByLabel('Connection name').waitFor();
    assert.equal(await advanced.getAttribute('open'), null);
    await page.getByLabel('Required network scope').waitFor({ state: 'visible' });
    await page.getByLabel('Required video interface').waitFor({ state: 'visible' });
    await page.getByRole('button', { name: 'Save fixture' }).click();
    await timeout.waitFor({ state: 'visible' });
    await timeout.fill('30');
    await advanced.locator('summary').focus();
    await page.keyboard.press('Enter');
    await timeout.waitFor({ state: 'hidden' });
    await page.keyboard.press('Enter');
    assert.equal(await timeout.inputValue(), '30');
    // Optional capabilities collapse, but adapter-required and unknown plugin
    // fields are never silently hidden. Disclosure retains entered values.
    await page.getByLabel('Plugin extension').waitFor({ state: 'visible' });
    const capabilities = page.locator('.operator-form-advanced').filter({ hasText: 'Optional capabilities' });
    assert.equal(await capabilities.getAttribute('open'), null);
    await capabilities.locator('summary').focus();
    await page.keyboard.press('Enter');
    await page.getByLabel('Image model').fill('image-example');
    await capabilities.locator('summary').click();
    await page.getByLabel('Image model').waitFor({ state: 'hidden' });
    await capabilities.locator('summary').click();
    assert.equal(await page.getByLabel('Image model').inputValue(), 'image-example');
    for (const theme of ['light', 'dark']) {
      await page.evaluate(value => { document.documentElement.dataset.theme = value; }, theme);
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
      assert.equal(await page.getByLabel('Connection name').evaluate(element => getComputedStyle(element).fontSize), '16px');
      assert.equal(await capabilities.locator('label').first().evaluate(element => getComputedStyle(element).color),
        await page.locator('.operator-form-section label').first().evaluate(element => getComputedStyle(element).color));
      const artifacts = fileURLToPath(new URL('../e2e-artifacts/upstream-availability', import.meta.url));
      mkdirSync(artifacts, { recursive: true });
      await page.screenshot({ path: `${artifacts}/provider-form-${theme}-mobile.png`, fullPage: true });
    }
    const editBeta = page.getByRole('button', { name: 'Edit Beta', exact: true });
    const editValue = page.getByLabel('Edit value');
    await editBeta.click();
    assert.equal(await editValue.evaluate(element => document.activeElement === element), true);
    await page.getByRole('button', { name: 'Cancel edit', exact: true }).click();
    assert.equal(await editBeta.evaluate(element => document.activeElement === element), true);
    await editBeta.click();
    await page.getByRole('button', { name: 'Save edit', exact: true }).click();
    assert.equal(await editValue.evaluate(element => document.activeElement === element), true);
    await editValue.fill('Edited');
    await page.getByRole('button', { name: 'Save edit', exact: true }).click();
    await page.getByRole('button', { name: 'Reject save', exact: true }).click();
    assert.equal(await editBeta.evaluate(element => document.activeElement === element), false);
    await editValue.waitFor({ state: 'visible' });
    await page.getByRole('button', { name: 'Save edit', exact: true }).click();
    await page.getByRole('button', { name: 'Accept save and refresh rows', exact: true }).click();
    await editValue.waitFor({ state: 'detached' });
    assert.equal(await editBeta.isDisabled(), true);
    await page.getByRole('button', { name: 'Finish refresh', exact: true }).click();
    assert.equal(await editBeta.evaluate(element => document.activeElement === element), true);
    const extension = page.locator('[data-layout-contract]');
    await extension.getByRole('note').getByText('This field cannot be edited visually yet', { exact: true }).waitFor();
    const visibleExtension = await extension.innerText();
    assert.doesNotMatch(visibleExtension, /schema-default-must-not-render|schema-example-must-not-render|existing-value-must-not-render|Unsupported field schema/);
    assert.equal(await extension.locator('pre').count(), 0);
    for (const width of [390, 1440]) {
      await page.setViewportSize({ width, height: 844 });
      for (const theme of ['light', 'dark']) {
        await page.evaluate(value => { document.documentElement.dataset.theme = value; }, theme);
        assert.equal(await extension.evaluate(element => element.scrollWidth <= element.clientWidth), true, 'unknown schema and array legends stay inside their form');
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
      }
    }
    await extension.getByRole('textbox').fill('edited');
    await extension.getByRole('button', { name: 'Save extended fields' }).click();
    const submitted = await page.evaluate(() => (window as Window & { formLayoutSubmission?: { future: string; entries: string[] } }).formLayoutSubmission);
    assert.deepEqual(submitted, { future: 'existing-value-must-not-render', entries: ['edited'] }, 'unsupported fields preserve existing data while supported siblings remain editable');
  } finally { await browser.close(); await server.close(); }
});
