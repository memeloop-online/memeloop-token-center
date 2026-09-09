import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

declare global { interface Window { sessionListReads: number; resolveSessionList: (ok: boolean) => void } }

test('session initial spinner, retry failure and background retry preserve the right visible page', { timeout: 45_000 }, async () => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    return test.skip('Chromium required');
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/session-loading.html`);
    await page.waitForFunction(() => window.sessionListReads === 1);
    await page.locator('.session-browser').getByText('Loading…', { exact: true }).waitFor();
    assert.equal(await page.getByText('Retained session', { exact: true }).count(), 0);
    await page.evaluate(() => window.resolveSessionList(false));
    const retry = page.getByRole('button', { name: 'Retry loading sessions', exact: true });
    await retry.waitFor();
    assert.equal(await page.locator('.session-browser').getByText('Loading…', { exact: true }).count(), 0);
    await retry.click();
    await page.waitForFunction(() => window.sessionListReads === 2);
    await page.locator('.session-browser').getByText('Loading…', { exact: true }).waitFor();
    await page.evaluate(() => window.resolveSessionList(true));
    await page.getByText('Retained session', { exact: true }).first().waitFor();
    await page.getByRole('button', { name: 'Simulate session event' }).click();
    await page.waitForFunction(() => window.sessionListReads === 3);
    assert.ok(await page.getByText('Retained session', { exact: true }).count() > 0);
    await page.evaluate(() => window.resolveSessionList(false));
    await retry.waitFor();
    await retry.click();
    await page.waitForFunction(() => window.sessionListReads === 4);
    assert.ok(await page.getByText('Retained session', { exact: true }).count() > 0, 'manual retry retains the loaded page while pending');
    await page.evaluate(() => window.resolveSessionList(false));
    await retry.waitFor();
    assert.ok(await page.getByText('Retained session', { exact: true }).count() > 0, 'failed retry does not blank the page');
    assert.equal(await page.locator('.session-browser').getByText('Loading…', { exact: true }).count(), 0);
  } finally { await browser.close(); await server.close(); }
});
