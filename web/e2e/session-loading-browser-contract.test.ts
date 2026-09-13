import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

declare global {
  interface Window {
    sessionDetailReads: number;
    sessionListReads: number;
    resolveSessionList: (ok: boolean) => void;
  }
}

async function settleRenderedRefresh(page: import('playwright').Page, expectedListReads: number) {
  await page.waitForFunction((expected) => window.sessionListReads === expected, expectedListReads);
  await page.locator('.session-live-state.live').waitFor();
  // Two animation frames form a deterministic browser render boundary: the
  // refresh promise and any detail request it starts have both left the
  // microtask queue before the assertion reads the fixture counters.
  await page.evaluate(() => new Promise<void>((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
}

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

test('session events refresh only the exact selected credential and session detail', { timeout: 45_000 }, async () => {
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
    await page.evaluate(() => window.resolveSessionList(true));
    await page.waitForFunction(() => window.sessionDetailReads === 1);

    await page.getByRole('button', { name: 'Simulate other credential event', exact: true }).click();
    await page.waitForFunction(() => window.sessionListReads === 2);
    await page.evaluate(() => window.resolveSessionList(true));
    await settleRenderedRefresh(page, 2);
    assert.equal(await page.evaluate(() => window.sessionDetailReads), 1,
      'another credential may update the tenant list but must not refresh the selected detail');

    await page.getByRole('button', { name: 'Simulate other session event', exact: true }).click();
    await page.waitForFunction(() => window.sessionListReads === 3);
    await page.evaluate(() => window.resolveSessionList(true));
    await settleRenderedRefresh(page, 3);
    assert.equal(await page.evaluate(() => window.sessionDetailReads), 1,
      'another session on the same credential must not refresh the selected detail');

    await page.getByRole('button', { name: 'Simulate archive bound event', exact: true }).click();
    await page.waitForFunction(() => window.sessionListReads === 4);
    await page.evaluate(() => window.resolveSessionList(true));
    await page.waitForFunction(() => window.sessionDetailReads === 2);
    assert.equal(await page.evaluate(() => window.sessionDetailReads), 2,
      'archive_bound refreshes the exact selected request although HTTP status and combined archive state stay unchanged');

    await page.getByRole('button', { name: 'Simulate session event', exact: true }).click();
    await page.waitForFunction(() => window.sessionListReads === 5);
    await page.evaluate(() => window.resolveSessionList(true));
    await page.waitForFunction(() => window.sessionDetailReads === 3);
    assert.equal(await page.evaluate(() => window.sessionDetailReads), 3,
      'the exact selected credential and session event refreshes its detail once');

    await page.getByRole('button', { name: 'Open Unlinked requests', exact: true }).click();
    await page.waitForFunction(() => window.sessionDetailReads === 4);
    await page.getByRole('button', { name: 'Simulate confirmed projection', exact: true }).click();
    await page.waitForFunction(() => window.sessionListReads === 6);
    await page.evaluate(() => window.resolveSessionList(true));
    await page.waitForFunction(() => window.sessionDetailReads === 5);
    assert.equal(await page.evaluate(() => window.sessionDetailReads), 5,
      'a confirmed projection refreshes the old same-credential unlinked detail');
  } finally { await browser.close(); await server.close(); }
});
