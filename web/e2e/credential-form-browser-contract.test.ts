import assert from 'node:assert/strict';
import { existsSync, mkdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

test('credential mode remains editable without raw routing arrays or mobile overflow', { timeout: 30_000 }, async (context) => {
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
    page.setDefaultTimeout(5_000);
    page.on('pageerror', (error) => context.diagnostic(error.message));
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/credential-form.html`);
    await page.locator('.rjsf').waitFor();
    const artifacts = fileURLToPath(new URL('../e2e-artifacts/upstream-availability', import.meta.url));
    mkdirSync(artifacts, { recursive: true });
    await page.screenshot({ path: `${artifacts}/credential-form-mobile.png`, fullPage: true });
    const mode = page.getByRole('combobox', { name: 'Metering and limit mode' });
    await mode.waitFor();
    assert.equal(await mode.inputValue(), 'prepaid');
    assert.equal(await page.getByText('Unsupported field schema', { exact: false }).count(), 0);
    assert.equal(await page.locator('.schema-array').count(), 0);
    await page.getByRole('textbox', { name: /^Alias/ }).fill('Example');
    await page.getByLabel('Extension field').fill('retained');
    await mode.selectOption('metered_unlimited');
    await page.getByRole('button', { name: 'Create fixture' }).click();
    await page.waitForFunction(() => Boolean(document.querySelector('output')?.textContent), undefined, { timeout: 5_000 });
    const submitted = JSON.parse(await page.locator('output').innerText());
    assert.equal(submitted.policy.enforcement_mode, 'metered_unlimited');
    assert.equal(submitted.extension, 'retained');
    assert.equal(submitted.route_ids, undefined);
    assert.equal(submitted.route_group_ids, undefined);
    for (const theme of ['light', 'dark']) {
      await page.evaluate(value => { document.documentElement.dataset.theme = value; }, theme);
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
      assert.equal(await mode.evaluate(element => getComputedStyle(element).fontSize), '16px');
    }
    await mode.selectOption('prepaid');
    await page.getByRole('button', { name: 'Create fixture' }).click();
    await page.waitForFunction(() => JSON.parse(document.querySelector('output')!.textContent!).policy.enforcement_mode === 'prepaid', undefined, { timeout: 5_000 });
  } finally { await browser.close(); await server.close(); }
});
