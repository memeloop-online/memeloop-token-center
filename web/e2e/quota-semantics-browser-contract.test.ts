import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer } from './support/isolated-vite-server.js';

test('failed quota refresh labels retained zeroes as historical and hides unobserved zeroes', { timeout: 30_000 }, async context => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    context.skip('Chromium required'); return;
  }
  const server = await createIsolatedFixtureServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const origin = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    await page.clock.install();
    const forbiddenRequests: string[] = [];
    const pageErrors: string[] = [];
    page.on('pageerror', error => pageErrors.push(error.message));
    await page.route('**/*', route => {
      const url = new URL(route.request().url());
      if (url.origin !== origin || url.pathname.startsWith('/internal/')) {
        forbiddenRequests.push(url.pathname); return route.abort();
      }
      return route.continue();
    });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`${origin}/e2e/fixtures/quota-semantics.html`);

    const summary = page.locator('[data-case="summary"]');
    assert.match(await summary.innerText(), /Codex usage · 5-hour limit · 51% used/);
    assert.equal(await summary.getByRole('meter').getAttribute('aria-valuenow'), '51');
    assert.match(await summary.getByRole('meter').getAttribute('aria-valuetext') ?? '', /51% used/);
    await summary.locator('[tabindex="0"]').focus();
    const windows = page.getByRole('tooltip').filter({ hasText: 'Weekly limit' });
    await windows.waitFor();
    assert.match(await windows.innerText(), /20%/);
    assert.match(await windows.innerText(), /Codex code review/);
    assert.match(await windows.innerText(), /—/);
    await page.keyboard.press('Escape');
    assert.match(await page.locator('[data-case="summary-retained"]').innerText(), /Refresh failed · last observed Codex usage · 5-hour limit 0% used/);
    assert.equal(await page.locator('[data-case="summary-unobserved"]').getByRole('meter').count(), 0);
    const expiring = page.locator('[data-case="summary-expiring"]');
    assert.doesNotMatch(await expiring.innerText(), /Last observed/);
    await page.clock.fastForward(6_000);
    await page.waitForFunction(() => document.querySelector('[data-case="summary-expiring"]')?.textContent?.includes('Last observed'));

    const retained = page.locator('[data-case="retained"]');
    const retainedText = await retained.innerText();
    assert.match(retainedText, /Codex usage · 5-hour limit/);
    assert.match(retainedText, /Last observed balance 0/);
    assert.match(retainedText, /Last observed · not a current result/);
    assert.match(retainedText, /This refresh failed/);
    assert.doesNotMatch(retainedText, /code:primary_window|codex_usage/);
    assert.equal(await retained.locator('meter').count(), 1);
    const unmapped = page.locator('[data-case="unmapped"]');
    assert.match(await unmapped.innerText(), /Codex additional usage · 5-hour limit/);
    assert.doesNotMatch(await unmapped.innerText(), /bengalfox|balance 0/i, 'an internal feature is not a model name; unsourced zero is not a balance');
    assert.doesNotMatch(await page.locator('[data-case="missing-balance"]').innerText(), /balance/i, 'a credits source without its balance field does not imply a zero balance');

    await retained.locator('[data-quota-evidence="code:primary_window"]').click();
    const evidence = page.getByRole('tooltip').filter({ hasText: /OpenAI Codex usage endpoint · Raw window ID: code:primary_window · Raw source: codex_usage/ });
    await evidence.waitFor();

    const unobserved = page.locator('[data-case="unobserved"]');
    const unobservedText = await unobserved.innerText();
    assert.match(unobservedText, /without a confirmed quota observation/);
    assert.doesNotMatch(unobservedText, /Last observed balance|Upstream balance|Codex usage · Weekly limit/);
    assert.equal(await unobserved.locator('meter').count(), 0);
    assert.equal(await unobserved.locator('.upstream-quota-window-heading strong').count(), 0);
    assert.deepEqual(forbiddenRequests, []);
    assert.deepEqual(pageErrors, []);
  } finally { await browser.close(); await server.close(); }
});
