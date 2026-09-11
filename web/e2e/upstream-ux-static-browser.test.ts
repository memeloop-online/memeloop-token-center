import assert from 'node:assert/strict';
import test from 'node:test';
import { chromium } from 'playwright';

test('quota card and confirmation render without activating any quota control', { skip: !process.env.MTC_UX_BASE_URL }, async () => {
  const base = process.env.MTC_UX_BASE_URL!;
  assert.match(base, /^http:\/\/127\.0\.0\.1:\d+$/);
  const browser = await chromium.launch();
  try {
    const page = await browser.newPage();
    const errors: string[] = [];
    page.on('pageerror', (error) => errors.push(error.message));
    await page.route('**/*', (route) => {
      const url = new URL(route.request().url());
      return url.origin !== base || url.pathname.startsWith('/internal/') ? route.abort() : route.continue();
    });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    for (const width of [390, 1440]) {
      await page.setViewportSize({ width, height: 1000 });
      await page.goto(`${base}/ui-assets/e2e/fixtures/upstream-ux-static.html`);
      await page.getByRole('meter').waitFor();
      assert.equal(await page.getByRole('meter').count(), 1);
      assert.equal(await page.getByRole('button', { name: 'Reset upstream quota', exact: true }).isEnabled(), true);
      assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth));
      await page.screenshot({ path: `/tmp/mtc-upstream-ux-static-quota-${width}.png`, fullPage: true });
      await page.goto(`${base}/ui-assets/e2e/fixtures/upstream-ux-static.html?confirm`);
      const dialog = page.getByRole('dialog');
      await dialog.waitFor();
      assert.match(await dialog.innerText(), /Mock Codex.*mock-account.*1 upstream reset credit/s);
      assert.equal(await dialog.getByRole('button', { name: 'Cancel', exact: true }).evaluate((node) => node === document.activeElement), true);
      await page.screenshot({ path: `/tmp/mtc-upstream-ux-static-confirm-${width}.png`, fullPage: true });
      await page.keyboard.press('Escape');
      await dialog.waitFor({ state: 'hidden' });
    }
    assert.deepEqual(errors, []);
  } finally { await browser.close(); }
});
