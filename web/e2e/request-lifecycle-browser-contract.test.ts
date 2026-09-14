import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';
declare global { interface Window { requestLifecycleFixture: { finish: () => void; hold: () => void; release: () => void; switchScope: () => void; filter: () => void; failQuery: () => void; held: boolean; queryHeld: boolean; detailCalls: number; scopeCommits: { scope: string; drawers: number }[] }; } }

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
    const page = await browser.newPage({ hasTouch: true });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/request-lifecycle.html`);
    const first = page.locator('tbody tr').filter({ has: page.getByText('model-a', { exact: true }) });
    await page.getByRole('tooltip').filter({ hasText: 'Background scope help' }).waitFor();
    await first.locator('.table-action').click();
    await page.locator('.drawer [data-outcome="running"]').waitFor();
    assert.equal(await page.getByRole('tooltip').filter({ hasText: 'Background scope help' }).count(), 0, 'other-scope floating content remains excluded while the drawer is open');
    assert.equal(await page.locator('#background-help').evaluate((element) => !!element.closest('[inert]')), true, 'background controls remain inert');
    await page.locator('.drawer .request-outcome').tap();
    const ownedTooltip = page.getByRole('tooltip').filter({ hasText: 'No terminal outcome is recorded yet' });
    await ownedTooltip.waitFor();
    assert.equal(await ownedTooltip.evaluate((element) => !!element.closest('.drawer-owned-portals') && !element.closest('[inert], [aria-hidden="true"]')), true, 'only the drawer-owned portal remains in the accessible modal subtree');
    await page.locator('.drawer .close').focus();
    await page.locator('.drawer .request-outcome').focus();
    await ownedTooltip.waitFor();
    assert.match(await page.locator('.drawer .request-diagnostics').first().innerText(), /Awaiting settlement/);
    const modelDetails = page.locator('.drawer .request-detail-wide .request-metadata-trigger').first();
    await modelDetails.tap();
    const metadata = page.locator('.request-metadata-surface');
    await metadata.waitFor({ state: 'visible' });
    assert.equal(await metadata.evaluate(element => !!element.closest('.drawer-owned-portals') && !element.closest('[inert], [aria-hidden="true"]')), true, 'technical metadata belongs to the current accessible drawer');
    await metadata.getByRole('button').first().focus();
    await page.keyboard.press('Escape');
    await metadata.waitFor({ state: 'detached' });
    assert.equal(await page.locator('.drawer').count(), 1, 'Escape closes the supplemental metadata, not the request drawer');
    await page.evaluate(() => window.requestLifecycleFixture.finish());
    await page.locator('.drawer [data-outcome="completed"]').waitFor();
    assert.equal(await page.evaluate(() => window.requestLifecycleFixture.detailCalls), 2);
    await page.evaluate(() => { window.requestLifecycleFixture.hold(); window.requestLifecycleFixture.finish(); });
    await page.waitForFunction(() => window.requestLifecycleFixture.held);
    await page.getByRole('button', { name: 'Close', exact: true }).click();
    await page.getByRole('tooltip').filter({ hasText: 'Background scope help' }).waitFor();
    assert.equal(await page.locator('.drawer-owned-portals').count(), 0, 'closing removes only the owned portal subtree');
    assert.equal(await page.locator('#background-help').evaluate((element) => !!element.closest('[inert], [aria-hidden="true"]')), false, 'closing restores background accessibility');
    await page.locator('tbody tr').filter({ has: page.getByText('model-b', { exact: true }) }).locator('.table-action').click();
    await page.getByRole('dialog', { name: 'model-b' }).waitFor();
    await page.evaluate(() => window.requestLifecycleFixture.release());
    await page.evaluate(() => new Promise<void>((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
    assert.equal(await page.getByRole('dialog', { name: 'model-b' }).count(), 1, 'late request-a response cannot replace request-b');
    await page.getByRole('button', { name: 'Close', exact: true }).click();
    await first.locator('.table-action').click();
    await page.getByRole('dialog', { name: 'model-a' }).waitFor();
    await page.evaluate(() => { window.requestLifecycleFixture.hold(); window.requestLifecycleFixture.finish(); });
    await page.waitForFunction(() => window.requestLifecycleFixture.held);
    await page.evaluate(() => window.requestLifecycleFixture.switchScope());
    await page.locator('[data-request-fixture-scope="tenant-b"] tbody tr').first().waitFor();
    assert.deepEqual(await page.evaluate(() => window.requestLifecycleFixture.scopeCommits.filter((entry) => entry.scope === 'tenant-b')), [{ scope: 'tenant-b', drawers: 0 }], 'the first new-scope layout commit must not expose old detail before effects clear it');
    await page.evaluate(() => window.requestLifecycleFixture.release());
    await page.evaluate(() => new Promise<void>((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
    assert.equal(await page.getByRole('dialog').count(), 0, 'late detail cannot reopen across tenant scope');
    await first.locator('.table-action').click();
    await page.getByRole('dialog', { name: 'model-a' }).waitFor();
    assert.equal(await page.locator('.load-more button').count(), 1, 'the old result must have a pagination cursor before replacement');
    await page.evaluate(() => window.requestLifecycleFixture.filter());
    await page.waitForFunction(() => window.requestLifecycleFixture.queryHeld);
    assert.equal(await page.locator('tbody tr').count(), 0, 'old rows are hidden while replacement is pending');
    assert.equal(await page.locator('.load-more button').count(), 0, 'old cursor is not actionable while replacement is pending');
    assert.equal(await page.getByRole('dialog').count(), 0, 'filter replacement closes the old request detail');
    await page.evaluate(() => window.requestLifecycleFixture.failQuery());
    await page.getByRole('alert').waitFor();
    assert.equal(await page.locator('tbody tr').count(), 0, 'a failed replacement must not restore stale rows');
    assert.equal(await page.locator('.load-more button').count(), 0, 'a failed replacement must not restore its cursor');
    assert.equal(await page.getByRole('dialog').count(), 0, 'a failed replacement must not reopen old detail');
    for (const theme of ['dark', 'light']) {
      await page.evaluate((value) => { document.documentElement.dataset.theme = value; }, theme);
      const surface = await page.locator('.request-page-surface').evaluate((element) => ({ border: getComputedStyle(element).borderTopWidth, shadow: getComputedStyle(element).boxShadow }));
      assert.deepEqual(surface, { border: '0px', shadow: 'none' });
    }
  } finally { await browser.close(); await server.close(); }
});
