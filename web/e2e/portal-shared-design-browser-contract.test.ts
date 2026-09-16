import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer } from './support/isolated-vite-server.js';

test('Portal shares Fluent controls, surfaces and metric styling across themes', { timeout: 60000 }, async () => {
  const server = await createIsolatedFixtureServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen(); const address = server.httpServer!.address(); assert.ok(address && typeof address !== 'string');
  const base = `http://127.0.0.1:${address.port}/e2e/fixtures/portal-shared-design.html`;
  const browser = await chromium.launch({ headless: true });
  const shots = fileURLToPath(new URL('../e2e-artifacts/ui-system/portal', import.meta.url)); await mkdir(shots, { recursive: true });
  const key = { key_id: 'fixture-key', alias: 'Shared design account', currency: 'USD', credential_generation: 1, created_at: 1, available_balance: '9223372036691.748787', policy: { enforcement_mode: 'prepaid', requests_per_minute: 60, tokens_per_minute: 6000, max_concurrency: 4, daily_budget: null, weekly_budget: null, lifetime_budget: null } };
  const budget = { limit: null, settled: '0', reserved: '0', remaining: null, reset_at: null };
  try {
    for (const theme of ['light', 'dark']) for (const width of [390, 1440]) {
      const page = await browser.newPage({ viewport: { width, height: 1000 } });
      const errors: string[] = []; page.on('pageerror', error => errors.push(error.message));
      await page.addInitScript(() => { localStorage.setItem('mtc-locale', 'en'); });
      await page.route('**/self/v1/**', route => {
        assert.equal(route.request().method(), 'GET', 'fixture allows no writes');
        const path = new URL(route.request().url()).pathname;
        const json = path.endsWith('/limits') ? { ...key, captured_at: 1, reserved_balance: '0', rpm: { limit: 60, used: 2, remaining: 58, reset_at: 1 }, tpm: { limit: 6000, used: 20, remaining: 5980, reset_at: 1 }, concurrency: { limit: 4, active: 0, remaining: 4 }, daily_budget: budget, weekly_budget: budget, lifetime_budget: budget } : path.endsWith('/key') ? key : path.includes('/stats') ? { key_id: key.key_id, summary: { total_requests: 12, successful_requests: 10, failed_requests: 2, input_tokens: 1000, output_tokens: 500, total_cost: '0.000001', costs: [] }, by_model: [], by_day: [], errors: [] } : [];
        return route.fulfill({ json });
      });
      await page.goto(base);
      await page.evaluate(theme => { document.documentElement.dataset.theme = theme; }, theme);
      const input = page.locator('.self-sign-in .fui-Input input');
      await input.fill('fixture-only-client-token');
      assert.equal(await input.getAttribute('type'), 'password');
      await page.locator('.credential-visibility').click(); assert.equal(await input.getAttribute('type'), 'text');
      await page.locator('.credential-visibility').click(); assert.equal(await input.getAttribute('type'), 'password');
      await page.locator('.self-sign-in button[type="submit"]').click();
      await page.locator('.self-overview .self-account-summary').waitFor();
      assert.equal(await page.locator('.self-overview > .mtc-data-surface').count(), 3);
      assert.equal(await page.locator('.self-overview > .panel').count(), 0, 'surface is reused, not nested in a second panel');
      assert.equal(await page.locator('.self-overview-metrics .metric').count(), 5);
      const metric = page.locator('.self-overview-metrics .metric').first();
      assert.equal(await metric.evaluate(el => el.classList.contains('analytics-metric')), true, 'Portal metrics reuse the shared analytics metric primitive');
      assert.equal(await metric.evaluate(el => getComputedStyle(el).borderTopWidth), '0px', 'Analytics metrics use the shared borderless surface');
      assert.equal(await metric.evaluate(el => getComputedStyle(el).borderTopLeftRadius), '8px', 'Analytics metrics retain the shared surface radius');
      assert.equal(await metric.evaluate(el => getComputedStyle(el).backgroundColor), theme === 'light' ? 'rgb(243, 247, 248)' : 'rgb(18, 40, 43)', 'Analytics metric surface follows the selected theme');
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false);
      assert.equal(await page.getByText('fixture-only-client-token', { exact: true }).count(), 0);
      await page.screenshot({ path: `${shots}/${theme}-${width}.png`, fullPage: true });
      assert.deepEqual(errors, []);
      await page.close();
    }
    const page = await browser.newPage();
    await page.goto(`${base}?navigation`);
    const tabs = page.getByRole('tab');
    await tabs.first().focus(); await page.keyboard.press('ArrowRight');
    assert.equal(await tabs.nth(1).evaluate(el => document.activeElement === el), true);
    await page.keyboard.press('Enter');
    await page.waitForFunction(() => document.querySelector('[data-testid="route"]')?.textContent === 'requests');
    await page.close();
  } finally { await browser.close(); await server.close(); }
});
