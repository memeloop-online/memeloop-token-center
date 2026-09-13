import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

test('Fluent foundation keeps theme, focus details and responsive surfaces accessible', { timeout: 60_000 }, async (t) => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required');
    t.skip('Chromium is not installed'); return;
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    const consoleErrors: string[] = [];
    page.on('console', message => { if (message.type() === 'error') consoleErrors.push(message.text()); });
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/fluent-foundation.html`);
    await page.getByRole('textbox', { name: 'Account name' }).waitFor();
    const colors: string[] = [];
    for (const theme of ['light', 'dark']) {
      await page.evaluate((value) => { document.documentElement.dataset.theme = value; }, theme);
      await page.waitForFunction((previous) => getComputedStyle(document.querySelector('.mtc-data-surface')!).backgroundColor !== previous, colors.at(-1) ?? '');
      colors.push(await page.locator('.mtc-data-surface').evaluate((el) => getComputedStyle(el).backgroundColor));
      for (const width of [320, 768, 1440]) {
        await page.setViewportSize({ width, height: 720 });
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
      }
    }
    assert.notEqual(colors[0], colors[1]);
    const trigger = page.getByRole('button', { name: 'Route details' });
    await trigger.focus();
    await page.getByRole('tooltip').waitFor();
    assert.equal(await page.getByRole('tooltip').innerText(), 'Route ID: 0123456789');
    assert.ok(await trigger.getAttribute('aria-describedby'));
    await page.keyboard.press('Escape');
    await page.getByRole('tooltip').waitFor({ state: 'hidden' });
    await trigger.click();
    await page.getByRole('tooltip').waitFor();
    await page.getByRole('button', { name: 'Save' }).focus();
    await page.getByRole('tooltip').waitFor({ state: 'hidden' });
    const advanced = page.getByRole('button', { name: '高级设置' });
    assert.equal(await advanced.getAttribute('aria-expanded'), 'false');
    await advanced.focus();
    await page.keyboard.press('Enter');
    assert.equal(await advanced.getAttribute('aria-expanded'), 'true');
    await page.getByText('网络代理').waitFor();
    assert.equal(await page.locator('details, summary').count(), 0);
    // External validation opens a controlled section and focuses its invalid field.
    const controlled = page.getByRole('button', { name: 'Controlled advanced settings' });
    const draft = page.getByRole('textbox', { name: 'Draft proxy' });
    assert.equal(await controlled.getAttribute('aria-expanded'), 'false');
    await page.getByRole('button', { name: 'Validate advanced settings' }).click();
    await draft.waitFor();
    await page.waitForFunction(() => document.activeElement?.getAttribute('aria-invalid') === 'true');
    assert.equal(await controlled.getAttribute('aria-expanded'), 'true');
    assert.equal(await draft.evaluate((el) => el === document.activeElement), true);
    const originalInput = await draft.elementHandle();
    assert.ok(originalInput);
    await page.keyboard.press('ControlOrMeta+A');
    await page.keyboard.type('unsaved proxy draft');
    await controlled.click();
    assert.equal(await controlled.getAttribute('aria-expanded'), 'false');
    // The same input stays mounted but both hidden and inert while collapsed.
    assert.equal(await originalInput.evaluate((el) => el.isConnected && !!el.closest('[hidden][inert]')), true);
    assert.equal(await originalInput.inputValue(), 'unsaved proxy draft');
    await page.keyboard.press('Enter');
    assert.equal(await controlled.getAttribute('aria-expanded'), 'true');
    assert.equal(await draft.inputValue(), 'unsaved proxy draft');
    await page.emulateMedia({ forcedColors: 'active', reducedMotion: 'reduce' });
    const surface = await page.locator('.mtc-data-surface').evaluate((el) => {
      const style = getComputedStyle(el);
      return { outline: style.outlineStyle, shadow: style.boxShadow, animation: style.animationDuration };
    });
    assert.equal(surface.outline, 'solid');
    assert.equal(surface.shadow, 'none');
    assert.equal(surface.animation, '1e-05s');
    assert.deepEqual(consoleErrors, [], 'controlled and uncontrolled disclosures must not emit React or Fluent errors');
  } finally { await browser.close(); await server.close(); }
});
