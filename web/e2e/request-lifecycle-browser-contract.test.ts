import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';
declare global { interface Window { requestLifecycleFixture: { finish: () => void; hold: () => void; release: () => void; switchScope: () => void; held: boolean; detailCalls: number }; } }

test('request detail follows terminal events and fences late responses after selection and scope changes', { timeout: 45_000 }, async (t) => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required');
    t.skip('Chromium is not installed'); return;
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/request-lifecycle.html`);
    const first = page.locator('tbody tr').filter({ has: page.getByText('model-a', { exact: true }) });
    await first.locator('.table-action').click();
    await page.locator('.drawer [data-outcome="running"]').waitFor();
    assert.match(await page.locator('.drawer .request-diagnostics').first().innerText(), /Awaiting settlement/);
    await page.evaluate(() => window.requestLifecycleFixture.finish());
    await page.locator('.drawer [data-outcome="completed"]').waitFor();
    assert.equal(await page.evaluate(() => window.requestLifecycleFixture.detailCalls), 2);
    await page.evaluate(() => { window.requestLifecycleFixture.hold(); window.requestLifecycleFixture.finish(); });
    await page.waitForFunction(() => window.requestLifecycleFixture.held);
    await page.getByRole('button', { name: 'Close', exact: true }).click();
    await page.locator('tbody tr').filter({ has: page.getByText('model-b', { exact: true }) }).locator('.table-action').click();
    await page.getByRole('dialog', { name: 'model-b' }).waitFor();
    await page.evaluate(() => window.requestLifecycleFixture.release());
    await page.evaluate(() => new Promise<void>((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
    assert.equal(await page.getByRole('dialog', { name: 'model-b' }).count(), 1, 'late request-a response cannot replace request-b');
    await page.getByRole('button', { name: 'Close', exact: true }).click();
    await page.evaluate(() => window.requestLifecycleFixture.hold());
    await first.locator('.table-action').click();
    await page.waitForFunction(() => window.requestLifecycleFixture.held);
    await page.evaluate(() => window.requestLifecycleFixture.switchScope());
    await page.locator('[data-request-fixture-scope="tenant-b"] tbody tr').first().waitFor();
    await page.evaluate(() => window.requestLifecycleFixture.release());
    await page.evaluate(() => new Promise<void>((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
    assert.equal(await page.getByRole('dialog').count(), 0, 'late detail cannot reopen across tenant scope');
    for (const theme of ['dark', 'light']) {
      await page.evaluate((value) => { document.documentElement.dataset.theme = value; }, theme);
      const surface = await page.locator('.request-page-surface').evaluate((element) => ({ border: getComputedStyle(element).borderTopWidth, shadow: getComputedStyle(element).boxShadow }));
      assert.deepEqual(surface, { border: '0px', shadow: 'none' });
    }
  } finally { await browser.close(); await server.close(); }
});
