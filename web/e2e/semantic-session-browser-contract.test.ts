import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

test('semantic viewer reads verified atoms lazily, preserves parts and pages without resident history growth', { timeout: 45_000 }, async () => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    return test.skip('Chromium required');
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen(); const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 390, height: 900 } });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/semantic-session.html`);
    await page.getByRole('button', { name: 'Read message · 2', exact: true }).waitFor();
    assert.equal(await page.evaluate(() => window.semanticAtomReads), 0);
    await page.getByText('tool_result_not_recorded', { exact: true }).waitFor();
    await page.getByRole('button', { name: 'Read message · 2', exact: true }).click();
    await page.getByText('Verified latest answer', { exact: true }).waitFor();
    await page.getByText('Separate reasoning', { exact: true }).waitFor();
    assert.match(await page.locator('.semantic-atom-content').innerText(), /"query": "structured"/);
    assert.equal(await page.evaluate(() => window.semanticAtomReads), 1);
    await page.getByRole('button', { name: 'Earlier messages', exact: true }).click();
    await page.getByRole('button', { name: 'Read message · 1', exact: true }).waitFor();
    assert.equal(await page.getByRole('button', { name: 'Read message · 2', exact: true }).count(), 0, 'old resident window is released');
    await page.getByRole('button', { name: 'Read message · 1', exact: true }).click();
    await page.getByText('Verified earlier answer', { exact: true }).waitFor();
    assert.equal(await page.evaluate(() => window.semanticAtomReads), 2);
    // Explicit refresh returns to the latest signed manifest without reading its body again.
    await page.getByRole('button', { name: 'Refresh conversation', exact: true }).click();
    await page.waitForFunction(() => window.semanticPageReads === 3);
    assert.equal(await page.evaluate(() => window.semanticAtomReads), 2);
    assert.equal(await page.locator('.semantic-atom-content').count(), 0);
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
    assert.equal(await page.getByRole('textbox').count(), 0, 'read-only viewer has no composer');
  } finally { await browser.close(); await server.close(); }
});
