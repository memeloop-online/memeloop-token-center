import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { mkdir } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

declare global {
  interface Window { quotaReads: number; quotaWrites: number }
}

test('quota loads only on demand and shows window/reset evidence without any reset mutation', { timeout: 90_000 }, async () => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    return test.skip('Chromium required');
  }
  const root = fileURLToPath(new URL('..', import.meta.url));
  const server = await createServer({ root, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    const url = `http://127.0.0.1:${address.port}/e2e/fixtures/upstream-quota.html`;
    await page.goto(url);
    await page.getByRole('button', { name: 'View quota', exact: true }).waitFor();
    assert.equal(await page.evaluate(() => window.quotaReads), 0);
    await page.getByRole('button', { name: 'View quota', exact: true }).click();
    await page.getByText('Primary window', { exact: true }).waitFor();
    assert.equal(await page.locator('meter').count(), 1, 'unknown usage never renders a zero/full meter');
    await page.getByText('The upstream supports reset; reset operations are not yet integrated here.', { exact: true }).waitFor();
    await page.getByText('Stale data', { exact: true }).waitFor();
    assert.equal(await page.getByRole('button', { name: /reset/i }).count(), 0);
    const artifacts = join(root, 'e2e-artifacts', 'upstream-quota');
    await mkdir(artifacts, { recursive: true });
    for (const theme of ['light', 'dark']) {
      await page.evaluate((value) => { document.documentElement.dataset.theme = value; }, theme);
      for (const width of [320, 390, 768, 1024, 1440, 1920, 2560]) {
        await page.setViewportSize({ width, height: 900 });
        assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth));
        await page.screenshot({ path: join(artifacts, `upstream-quota-${theme}-${width}.png`), fullPage: true });
      }
    }
    assert.equal(await page.evaluate(() => window.quotaWrites), 0);
    for (const mode of ['error', 'unsupported']) {
      await page.goto(`${url}?mode=${mode}`);
      await page.getByRole('button', { name: 'View quota', exact: true }).click();
      await page.getByText(mode === 'error' ? 'Could not read quota. Try again.' : 'The upstream does not support quota reset.', { exact: true }).waitFor();
      assert.equal(await page.evaluate(() => window.quotaWrites), 0);
    }
  } finally {
    await browser.close();
    await server.close();
  }
});
