import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { mkdir } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

declare global { interface Window { quotaReads: number; quotaWrites: number; quotaPrepares: number; quotaConfirms: number; quotaStatuses: number; quotaReconciles: number } }

test('upstream themes and mock-only quota demand, consent and reconciliation contract', { timeout: 90_000 }, async () => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    return test.skip('Chromium required');
  }
  const root = fileURLToPath(new URL('..', import.meta.url));
  const server = await createServer({ root, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const base = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    const errors: string[] = [];
    page.on('pageerror', (error) => errors.push(error.message));
    await page.route('**/*', (route) => {
      const url = new URL(route.request().url());
      return url.origin !== base || url.pathname.startsWith('/internal/') ? route.abort() : route.continue();
    });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    const url = `${base}/e2e/fixtures/upstream-ux-static.html`;
    const artifacts = join(root, 'e2e-artifacts', 'upstream-quota');
    await mkdir(artifacts, { recursive: true });
    await page.goto(url);
    assert.equal(await page.getByLabel('Base URL', { exact: true }).getAttribute('readonly'), '');
    await page.getByText('3. Advanced network and retry policy', { exact: true }).focus();
    await page.keyboard.press('Enter');
    await page.getByLabel('Connect attempts', { exact: true }).waitFor();
    await page.getByRole('button', { name: 'Configure network proxy', exact: true }).click();
    const proxy = page.locator('.upstream-proxy-editor input');
    await proxy.fill('socks5://100.64.0.16:1080');
    assert.equal(await proxy.getAttribute('aria-invalid'), 'true');
    await proxy.fill('socks5h://100.64.0.16:1080');
    assert.equal(await page.getByRole('button', { name: 'Save network proxy', exact: true }).isEnabled(), true);
    // Do not save or activate any quota operation in this no-network fixture.
    await page.getByRole('button', { name: 'Cancel', exact: true }).click();
    await page.getByText('Stale data', { exact: true }).waitFor();
    assert.equal(await page.getByRole('meter').count(), 1, 'unknown usage never renders a zero/full meter');
    assert.equal(await page.getByRole('button', { name: 'Reset upstream quota', exact: true }).isEnabled(), true);
    for (const [theme, width] of [['light', 1440], ['dark', 390]] as const) {
      await page.evaluate((value) => { document.documentElement.dataset.theme = value; }, theme);
      const styles = await page.evaluate(() => {
        const border = getComputedStyle(document.querySelector('.upstream-connection')!);
        const label = getComputedStyle(document.querySelector('.upstream-connection dt')!);
        const advanced = getComputedStyle(document.querySelector('.upstream-advanced')!);
        return { border: border.borderTopColor, width: border.borderTopWidth, style: border.borderTopStyle, muted: label.color, advanced: advanced.borderTopColor };
      });
      assert.deepEqual(styles, { border: theme === 'light' ? 'rgb(195, 213, 219)' : 'rgb(61, 105, 113)', width: '1px', style: 'solid', muted: theme === 'light' ? 'rgb(82, 105, 112)' : 'rgb(145, 170, 176)', advanced: theme === 'light' ? 'rgb(195, 213, 219)' : 'rgb(61, 105, 113)' });
        await page.setViewportSize({ width, height: 900 });
        assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth));
        await page.screenshot({ path: join(artifacts, `upstream-quota-${theme}-${width}.png`), fullPage: true });
    }
    // Exercise the real UpstreamQuota, with fetch replaced by the strict local fixture.
    const quotaUrl = `${base}/e2e/fixtures/upstream-quota.html?mode=unknown`;
    await page.goto(quotaUrl);
    const view = page.getByRole('button', { name: 'View quota', exact: true });
    await view.waitFor();
    assert.deepEqual(await page.evaluate(() => [window.quotaReads, window.quotaWrites]), [0, 0]);
    await view.click();
    await page.getByText('Primary window', { exact: true }).waitFor();
    await page.locator('.upstream-danger-zone > summary').click();
    const reset = page.getByRole('button', { name: 'Reset upstream quota', exact: true });
    await reset.hover();
    assert.deepEqual(await page.evaluate(() => [window.quotaReads, window.quotaPrepares, window.quotaConfirms, window.quotaWrites]), [1, 0, 0, 0]);
    await reset.click();
    const dialog = page.getByRole('dialog');
    await dialog.waitFor();
    assert.match(await dialog.innerText(), /quota-account.*1 upstream reset credit/s);
    assert.equal(await dialog.getByRole('button', { name: 'Cancel', exact: true }).evaluate((node) => node === document.activeElement), true);
    await page.screenshot({ path: join(artifacts, 'upstream-quota-confirm-mobile.png'), fullPage: true });
    await dialog.getByRole('button', { name: 'Cancel', exact: true }).click();
    await dialog.waitFor({ state: 'hidden' });
    const status = page.getByRole('button', { name: 'Check operation status', exact: true });
    await status.waitFor();
    assert.deepEqual(await page.evaluate(() => [window.quotaPrepares, window.quotaConfirms, window.quotaWrites]), [1, 0, 1], 'cancel permits only non-consuming preparation');
    assert.equal(await reset.count(), 0, 'cancelled preparation remains locked');
    // Fresh mount starts an independent operation; only explicit confirmation consumes.
    await page.goto(quotaUrl);
    await view.click();
    await page.locator('.upstream-danger-zone > summary').click();
    await reset.click();
    await dialog.getByRole('button', { name: 'Confirm and continue', exact: true }).click();
    const reconcile = page.getByRole('button', { name: 'Reconcile upstream quota (read only)', exact: true });
    await reconcile.waitFor();
    assert.deepEqual(await page.evaluate(() => [window.quotaPrepares, window.quotaConfirms]), [1, 1]);
    await status.click();
    await page.waitForFunction(() => window.quotaStatuses === 1 && !document.querySelector<HTMLButtonElement>('.upstream-quota-reset-action button')?.disabled);
    await reconcile.click();
    await page.waitForFunction(() => window.quotaReconciles === 1 && !document.querySelector<HTMLButtonElement>('.upstream-quota-reset-action button')?.disabled);
    assert.deepEqual(await page.evaluate(() => [window.quotaPrepares, window.quotaConfirms, window.quotaStatuses, window.quotaReconciles, window.quotaWrites]), [1, 1, 1, 1, 3], 'inspection never repeats preparation or consumption');
    assert.equal(await reset.count(), 0);
    // Read failures are mock-only; no reset/prepare/reconcile calls are made.
    for (const [mode, message] of [
      ['stale-error', 'Quota destination validation failed. Check this account’s endpoint, proxy and DNS configuration.'],
      ['rate-limited', 'The supplier rate-limited quota reading. Retry manually later; this does not mean quota is exhausted.'],
      ['permission', 'Your current credential cannot read upstream quota for this tenant. Check your sign-in and read permissions.'],
    ]) {
      await page.goto(`${base}/e2e/fixtures/upstream-quota.html?mode=${mode}`);
      await view.click();
      await page.getByRole('alert').getByText(message, { exact: true }).waitFor();
      if (mode !== 'permission') {
        await page.getByText('Primary window', { exact: true }).waitFor();
        await page.getByText('Refresh failed. The previous result remains below and may not reflect current quota. Retry manually.', { exact: true }).waitFor();
        assert.equal(await page.getByRole('meter').count(), 1);
      }
      assert.equal(await page.getByText('fixture-sensitive-message-must-not-render').count(), 0);
      assert.deepEqual(await page.evaluate(() => [window.quotaReads, window.quotaWrites, window.quotaPrepares, window.quotaConfirms, window.quotaReconciles]), [1, 0, 0, 0, 0]);
    }
    assert.deepEqual(errors, []);
  } finally {
    await browser.close();
    await server.close();
  }
});
