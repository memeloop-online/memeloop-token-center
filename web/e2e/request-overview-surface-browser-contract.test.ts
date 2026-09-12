import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

test('request popover stays non-modal across themes, locales and widths; models retain distinct account facts', { timeout: 60_000 }, async (t) => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required for request overview surface contracts');
    t.skip('Chromium is not installed');
    return;
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    for (const locale of ['en', 'zh-CN']) {
      const page = await browser.newPage();
      await page.addInitScript((value) => localStorage.setItem('mtc-locale', value), locale);
      await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/request-overview-surface.html`);
      const trigger = page.getByRole('button', { name: locale === 'en' ? 'Filter' : '筛选', exact: true });
      await trigger.waitFor();
      assert.equal(await page.locator('[data-upstream-model="shared-model"]').count(), 1);
      assert.equal(await page.locator('[data-upstream-account-id]').count(), 3);
      for (const name of ['Copilot', 'Cursor', 'Kimi']) assert.equal(await page.getByText(name, { exact: true }).count(), 1);
      for (const theme of ['dark', 'light']) {
        await page.evaluate((value) => { document.documentElement.dataset.theme = value; }, theme);
        for (const width of [320, 390, 768, 1024, 1440, 1920, 2560]) {
          await page.setViewportSize({ width, height: 900 });
          await trigger.click();
          const panel = page.locator('.typed-filter-dialog');
          await panel.waitFor();
          const surface = await panel.evaluate((element) => {
            const bounds = element.getBoundingClientRect();
            return { open: element.matches(':popover-open'), modal: element.hasAttribute('aria-modal'),
              left: bounds.left, right: bounds.right, background: getComputedStyle(element).backgroundColor,
              backdrop: getComputedStyle(element, '::backdrop').backgroundColor,
              overflow: document.documentElement.scrollWidth > innerWidth };
          });
          assert.equal(surface.open, true);
          assert.equal(surface.modal, false);
          assert.equal(surface.backdrop, 'rgba(0, 0, 0, 0)');
          assert.equal(surface.background, theme === 'light' ? 'rgb(255, 255, 255)' : 'rgb(13, 28, 32)');
          assert.ok(surface.left >= 0 && surface.right <= width);
          assert.equal(surface.overflow, false);
          await page.keyboard.press('Escape');
          assert.equal(await panel.count(), 0);
          assert.equal(await trigger.evaluate((element) => document.activeElement === element), true);
          await trigger.click();
          // Native popovers must not trap focus or make the rest of the page inert.
          const outside = page.locator('#outside-control');
          await outside.focus();
          assert.equal(await outside.evaluate((element) => document.activeElement === element), true);
          // Click an uncovered outside point; light dismissal must synchronize React state.
          await page.mouse.click(width - 2, 2);
          await panel.waitFor({ state: 'detached' });
          assert.equal(await trigger.getAttribute('aria-expanded'), 'false');
        }
      }
      await page.close();
    }
  } finally {
    await browser.close();
    await server.close();
  }
});
