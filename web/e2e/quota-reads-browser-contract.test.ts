import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';
declare global { interface Window { quotaReadCalls: string[]; quotaReadTriggers: string[]; quotaBatchBodies: unknown[]; quotaActive: number; quotaPeak: number; quotaUnexpectedWrites: number; releaseQuota: (index: number, status?: number) => void } }

test('quota rows start together, settle independently and retain failed observations', { timeout: 60_000 }, async () => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    return test.skip('Chromium required');
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    const errors: string[] = []; page.on('pageerror', error => errors.push(error.message));
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    const origin = `http://127.0.0.1:${address.port}`;
    await page.route('**/*', route => new URL(route.request().url()).origin === origin && !new URL(route.request().url()).pathname.startsWith('/internal/') ? route.continue() : route.abort());
    const url = `${origin}/e2e/fixtures/quota-reads.html`;
    await page.goto(url);
    await page.getByRole('button', { name: 'Duplicate read', exact: true }).click();
    await page.waitForFunction(() => window.quotaReadCalls.length === 1);
    await page.getByRole('button', { name: 'Refresh all upstream quotas', exact: true }).click();
    await page.waitForFunction(() => window.quotaReadCalls.length === 5);
    assert.equal(await page.evaluate(() => window.quotaPeak), 5);
    assert.deepEqual(await page.evaluate(() => window.quotaBatchBodies), []);
    assert.deepEqual(await page.evaluate(() => window.quotaReadTriggers), ['manual', 'bulk', 'bulk', 'bulk', 'bulk']);
    await page.evaluate(() => window.releaseQuota(3));
    await page.getByText('1/5', { exact: true }).waitFor();
    await page.locator('[data-account="account-3"] [data-reset-credit-expiry="known"]').first().waitFor();
    assert.match(await page.locator('[data-account="account-0"] [data-summary]').innerText(), /Refreshing/);
    await page.locator('[data-account="account-3"]').getByRole('button', { name: 'Refresh quota', exact: true }).click();
    await page.waitForFunction(() => window.quotaReadCalls.length === 6);
    await page.evaluate(() => window.releaseQuota(5, 504));
    await page.locator('[data-account="account-3"] [data-quota-error-code="quota_timeout"]').waitFor();
    assert.ok(await page.locator('[data-account="account-3"] [data-reset-credit-expiry="known"]').count() > 0);
    assert.match(await page.locator('[data-account="account-3"] [data-summary]').innerText(), /failed/i);
    await page.evaluate(() => { window.releaseQuota(0); window.releaseQuota(1); window.releaseQuota(2, 503); window.releaseQuota(4); });
    await page.getByText('5/5', { exact: true }).waitFor();
    assert.match(await page.locator('[data-account="account-2"] [data-summary]').innerText(), /failed/i);
    assert.equal(await page.evaluate(() => window.quotaUnexpectedWrites), 0);

    await page.goto(url);
    await page.getByRole('button', { name: 'Refresh all upstream quotas', exact: true }).click();
    await page.waitForFunction(() => window.quotaReadCalls.length === 5);
    await page.getByRole('button', { name: 'Disable second', exact: true }).click();
    await page.evaluate(() => { for (let index = 0; index < 5; index++) window.releaseQuota(index); });
    await page.getByText('5/5', { exact: true }).waitFor();
    assert.equal(await page.locator('[data-account="account-1"] [data-reset-credit-expiry]').count(), 0);

    await page.goto(url);
    await page.getByRole('button', { name: 'Duplicate read', exact: true }).click();
    await page.waitForFunction(() => window.quotaReadCalls.length === 1);
    await page.getByRole('button', { name: 'Change generation', exact: true }).click();
    await page.evaluate(() => window.releaseQuota(0));
    await page.waitForFunction(() => window.quotaActive === 0);
    assert.equal(await page.locator('[data-reset-credit-expiry]').count(), 0);
    await page.getByRole('button', { name: 'Duplicate read', exact: true }).click();
    await page.waitForFunction(() => window.quotaReadCalls.length === 2);
    await page.getByRole('button', { name: 'Change tenant', exact: true }).click();
    await page.evaluate(() => window.releaseQuota(1));
    await page.waitForFunction(() => window.quotaActive === 0);
    assert.equal(await page.locator('[data-reset-credit-expiry]').count(), 0);
    assert.deepEqual(errors, []);
  } finally { await browser.close(); await server.close(); }
});
