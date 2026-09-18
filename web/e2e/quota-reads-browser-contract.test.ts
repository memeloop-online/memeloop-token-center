import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';
declare global { interface Window { quotaReadCalls: string[]; quotaReadTriggers: string[]; quotaBatchBodies: unknown[]; quotaActive: number; quotaPeak: number; quotaUnexpectedWrites: number; releaseQuota: (index: number, status?: number) => void } }

test('quota reads share ownership, bounded batches, partial failures and credit expiry', { timeout: 60_000 }, async () => {
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
    assert.equal(await page.evaluate(() => window.quotaReadCalls.length), 0);
    await page.getByRole('button', { name: 'Refresh all upstream quotas', exact: true }).click();
    await page.waitForFunction(() => window.quotaReadCalls.length === 1);
    assert.equal(await page.locator('[data-summary]').filter({ hasText: 'Waiting to refresh' }).count(), 5);
    assert.equal(await page.getByRole('button', { name: 'Refresh all upstream quotas', exact: true }).isDisabled(), true);
    assert.deepEqual(await page.evaluate(() => window.quotaBatchBodies[0]), { account_ids: ['account-0', 'account-1', 'account-2', 'account-3', 'account-4'], fresh: true, trigger: 'bulk' });
    await page.evaluate(() => window.releaseQuota(0));
    await page.getByText('5/5', { exact: true }).waitFor();
    assert.equal(await page.evaluate(() => window.quotaPeak), 1);
    assert.deepEqual(await page.evaluate(() => window.quotaReadTriggers), []);
    await page.locator('[data-account="account-0"] [data-reset-credit-expiry="known"]').first().waitFor();
    await page.locator('[data-account="account-1"] [data-reset-credit-expiry="unknown"]').first().waitFor();
    assert.match(await page.locator('[data-account="account-2"] [data-summary]').innerText(), /failed/i);
    await page.locator('[data-account="account-2"]').getByRole('button', { name: 'Refresh quota', exact: true }).click();
    await page.waitForFunction(() => window.quotaReadCalls.length === 2);
    await page.evaluate(() => window.releaseQuota(1));
    await page.locator('[data-account="account-2"] [data-reset-credit-expiry="known"]').first().waitFor();
    assert.equal(await page.evaluate(() => window.quotaReadTriggers[0]), 'manual');
    await page.getByRole('button', { name: 'Refresh all upstream quotas', exact: true }).click();
    await page.waitForFunction(() => window.quotaReadCalls.length === 3);
    await page.evaluate(() => window.releaseQuota(2));
    await page.getByText('5/5', { exact: true }).waitFor();
    assert.match(await page.locator('[data-account="account-2"] [data-summary]').innerText(), /failed/i);
    assert.equal(await page.locator('[data-account="account-2"] [data-reset-credit-expiry="known"]').count(), 1, 'a failed batch item retains the previous successful snapshot');
    assert.match(await page.locator('[data-account="account-3"] [data-summary]').innerText(), /failed/i);
    assert.equal(await page.locator('[data-account="account-3"] [data-reset-credit-expiry="known"]').count(), 1, 'an unobserved supplier error retains the previous quota evidence');
    assert.equal(await page.evaluate(() => window.quotaUnexpectedWrites), 0);

    await page.goto(url);
    await page.getByRole('button', { name: 'Refresh all upstream quotas', exact: true }).click();
    await page.waitForFunction(() => window.quotaReadCalls.length === 1);
    await page.getByRole('button', { name: 'Disable second', exact: true }).click();
    await page.evaluate(() => window.releaseQuota(0));
    await page.getByText('5/5', { exact: true }).waitFor();
    assert.equal(await page.locator('[data-summary]').filter({ hasText: 'Waiting to refresh' }).count(), 0);
    assert.equal(await page.locator('[data-account="account-1"] [data-reset-credit-expiry]').count(), 0, 'an account disabled while the batch is in flight does not publish its response');

    await page.goto(url);
    await page.getByRole('button', { name: 'Refresh all upstream quotas', exact: true }).click();
    await page.waitForFunction(() => window.quotaReadCalls.length === 1);
    await page.getByRole('button', { name: 'Disable first', exact: true }).click();
    await page.evaluate(() => window.releaseQuota(0));
    await page.getByText('5/5', { exact: true }).waitFor();
    assert.equal(await page.locator('[data-account="account-0"] [data-summary]').innerText(), 'Not checked', 'a disabled in-flight account clears its busy state without publishing the response');
    assert.equal(await page.locator('[data-summary]').filter({ hasText: 'Refreshing quota' }).count(), 0);

    await page.goto(url);
    await page.getByRole('button', { name: 'Duplicate read', exact: true }).click();
    await page.waitForFunction(() => window.quotaReadCalls.length === 1);
    await page.getByRole('button', { name: 'Change generation', exact: true }).click();
    await page.evaluate(() => window.releaseQuota(0));
    await page.waitForFunction(() => window.quotaActive === 0);
    assert.equal(await page.locator('[data-reset-credit-expiry]').count(), 0, 'late old-generation snapshot never publishes');
    await page.getByRole('button', { name: 'Duplicate read', exact: true }).click();
    await page.waitForFunction(() => window.quotaReadCalls.length === 2);
    await page.getByRole('button', { name: 'Change tenant', exact: true }).click();
    await page.evaluate(() => window.releaseQuota(1));
    await page.waitForFunction(() => window.quotaActive === 0);
    assert.equal(await page.locator('[data-reset-credit-expiry]').count(), 0, 'late response ignoring Abort cannot cross tenant');
    assert.deepEqual(errors, []);
  } finally { await browser.close(); await server.close(); }
});
