import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { hasDefaultChromium } from './support/browser-executable.js';
import { createServer } from 'vite';

declare global { interface Window { semanticAtomReads: number; semanticPageReads: number } }

test('semantic viewer reads verified atoms lazily, preserves parts and pages without resident history growth', { timeout: 45_000 }, async (context) => {
  if (!hasDefaultChromium(context)) return;
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen(); const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 390, height: 900 } });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/semantic-session.html`);
    await page.getByRole('button', { name: 'Read message · 2', exact: true }).waitFor();
    assert.equal(await page.locator('[data-semantic-role=assistant]').count(), 1);
    assert.equal(await page.locator('[data-semantic-role=agent]').count(), 0);
    assert.equal(await page.evaluate(() => window.semanticAtomReads), 0);
    await page.getByText('tool_result_not_recorded', { exact: true }).waitFor();
    await page.getByRole('button', { name: 'Read message · 2', exact: true }).click();
    await page.getByText('Verified latest answer', { exact: true }).waitFor();
    await page.getByRole('button', { name: 'Reasoning', exact: true }).click();
    await page.getByText('Separate reasoning', { exact: true }).waitFor();
    await page.getByRole('button', { name: 'Tool call · lookup', exact: true }).click();
    assert.match(await page.locator('.semantic-atom-content').innerText(), /"query": "structured"/);
    assert.equal(await page.evaluate(() => window.semanticAtomReads), 1);
    await page.getByRole('button', { name: 'Refresh conversation', exact: true }).click();
    await page.waitForFunction(() => window.semanticPageReads === 2);
    await page.getByText('Verified latest answer', { exact: true }).waitFor();
    assert.equal(await page.evaluate(() => window.semanticAtomReads), 1, 'same revision preserves the open atom');
    const artifacts = fileURLToPath(new URL('../e2e-artifacts/semantic-viewer/', import.meta.url));
    await mkdir(artifacts, { recursive: true });
    for (const theme of ['light', 'dark']) {
      await page.evaluate(value => { document.documentElement.dataset.theme = value; }, theme);
      await page.locator(`[data-semantic-theme=${theme}]`).waitFor();
      for (const width of [390, 1440]) {
        await page.setViewportSize({ width, height: 900 });
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
        await page.screenshot({ path: join(artifacts, `semantic-${theme}-${width}.png`), fullPage: true });
      }
    }
    await page.setViewportSize({ width: 390, height: 900 });
    await page.getByRole('button', { name: 'Earlier messages', exact: true }).click();
    await page.getByRole('button', { name: 'Read message · 1', exact: true }).waitFor();
    assert.equal(await page.getByRole('button', { name: 'Read message · 2', exact: true }).count(), 0, 'old resident window is released');
    assert.equal(await page.locator('[data-semantic-role=user]').count(), 1);
    await page.getByRole('button', { name: 'Read message · 1', exact: true }).click();
    await page.getByText('Verified earlier answer', { exact: true }).waitFor();
    assert.equal(await page.evaluate(() => window.semanticAtomReads), 2);
    // Explicit refresh returns to the latest signed manifest without reading its body again.
    await page.getByRole('button', { name: 'Refresh conversation', exact: true }).click();
    await page.waitForFunction(() => window.semanticPageReads === 4);
    assert.equal(await page.evaluate(() => window.semanticAtomReads), 2);
    assert.equal(await page.locator('.semantic-atom-content').count(), 0);
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
    assert.equal(await page.getByRole('textbox').count(), 0, 'read-only viewer has no composer');
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/semantic-session.html?scenario=unknown`);
    await page.getByRole('button', { name: 'Read message · 2', exact: true }).click();
    await page.getByText('Verified latest answer', { exact: true }).waitFor();
    assert.equal(await page.locator('[data-semantic-role=agent]').count(), 0);
    assert.equal(await page.locator('[data-semantic-role=unrecorded]').count(), 1);
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/semantic-session.html?scenario=error`);
    await page.getByText('Semantic read HTTP 500', { exact: true }).waitFor();
    assert.equal(await page.getByText('semantic_publication_pending', { exact: true }).count(), 0);
    assert.equal(await page.getByText('The semantic record is not fully published.', { exact: true }).count(), 0);
  } finally { await browser.close(); await server.close(); }
});
