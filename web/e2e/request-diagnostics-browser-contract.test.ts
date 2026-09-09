import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { readdir } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

import { chromium } from 'playwright';
import { createServer } from 'vite';

const webRoot = fileURLToPath(new URL('..', import.meta.url));
const requestId = '5a3bc0cc-8d47-4cee-9b5e-2581f8d99d13';
const sessionId = '726d4c8a-6641-4cb3-98bf-b21a64e4208f';
const upstreamId = 'e82ea007-9b7f-4be9-bf18-6829426a94e5';
const routeId = 'a75fc2f6-e145-4596-bc94-9736271c6d7e';

async function localChromiumExecutable() {
  const defaultExecutable = chromium.executablePath();
  if (existsSync(defaultExecutable)) return defaultExecutable;
  const workspaceUserCache = fileURLToPath(new URL('../../../../.cache/ms-playwright', import.meta.url));
  const installations = await readdir(workspaceUserCache, { withFileTypes: true }).catch(() => []);
  for (const installation of installations) {
    if (!installation.isDirectory() || !installation.name.startsWith('chromium-')) continue;
    const executable = join(workspaceUserCache, installation.name, 'chrome-linux64', 'chrome');
    if (existsSync(executable)) return executable;
  }
  return undefined;
}

test('Request diagnostics remain copyable, session-linked, and contained on narrow light and dark surfaces', { timeout: 30_000 }, async () => {
  const executablePath = await localChromiumExecutable();
  if (!executablePath) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required for the request diagnostics browser gate');
    return test.skip('a local Chromium runtime is required for request diagnostics layout assertions');
  }
  const server = await createServer({ root: webRoot, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0, strictPort: false } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ executablePath, headless: true });
  const context = await browser.newContext();
  try {
    const origin = `http://127.0.0.1:${address.port}`;
    // Establish clipboard permissions before loading the fixture document.
    await context.grantPermissions(['clipboard-read', 'clipboard-write'], { origin });
    const page = await context.newPage();
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`${origin}/e2e/fixtures/request-diagnostics.html`);
    const recorded = page.locator('[data-fixture-request="recorded"]');
    const historicalGap = page.locator('[data-fixture-request="historical-gap"]');
    const recordedDiagnostics = recorded.locator('.request-diagnostics');
    const historicalGapDiagnostics = historicalGap.locator('.request-diagnostics');
    await Promise.all([recordedDiagnostics.waitFor(), historicalGapDiagnostics.waitFor()]);

    const recordedRow = page.locator('tbody tr').filter({ has: page.locator(`code[title="${requestId}"]`) });
    assert.equal(await recordedRow.count(), 1);
    const tableId = recordedRow.locator('.request-id-control.compact code');
    assert.equal(await tableId.getAttribute('title'), requestId);
    assert.equal(await tableId.textContent(), requestId);
    assert.match(await recordedRow.locator('.request-token-cell small').textContent() ?? '', /Cache read 40.*Cache write 20/);
    assert.match(await recorded.textContent() ?? '', /http_429/);
    assert.match(await recorded.textContent() ?? '', new RegExp(upstreamId));
    assert.match(await recorded.textContent() ?? '', new RegExp(routeId));
    const historicalGapText = await historicalGap.textContent() ?? '';
    assert.match(historicalGapText, /Completed at—/);
    assert.match(historicalGapText, /Final upstream ID—/);
    assert.match(historicalGapText, /Final route ID—/);
    assert.doesNotMatch(historicalGapText, /Cache read|Cache write/, 'missing historical cache fields must remain absent rather than becoming zero-valued rows');
    assert.equal((await page.locator('tbody tr').nth(1).locator('td').nth(7).textContent())?.trim(), '—', 'an explicit historical null currency must not inherit the current credential currency');

    assert.equal(await page.evaluate(() => typeof navigator.clipboard?.writeText), 'function', 'the fixture must exercise the browser Clipboard API');
    await recordedRow.locator('.request-id-control.compact .copy-control button').click();
    await recordedRow.getByRole('button', { name: 'Copied', exact: true }).waitFor({ timeout: 5_000 });
    const copiedRequestId = await page.evaluate(async () => new Promise<string>((resolve, reject) => {
      const timeout = window.setTimeout(() => reject(new Error('clipboard read did not settle')), 5_000);
      void navigator.clipboard.readText().then((value) => { window.clearTimeout(timeout); resolve(value); }, (reason) => { window.clearTimeout(timeout); reject(reason); });
    }));
    assert.equal(copiedRequestId, requestId);

    await recordedRow.locator('.request-session-cell .table-link').click();
    assert.equal(await page.locator('[data-fixture-session-opened]').textContent(), sessionId);

    for (const theme of ['dark', 'light'] as const) {
      await page.evaluate((value) => { document.documentElement.dataset.theme = value; }, theme);
      for (const width of [320, 390, 768]) {
        await page.setViewportSize({ width, height: 900 });
        await page.evaluate(() => new Promise<void>((resolve) => requestAnimationFrame(() => resolve())));
        const layout = await page.evaluate(() => {
          const tableScroller = document.querySelector<HTMLElement>('.table-scroll')!;
          const compactId = document.querySelector<HTMLElement>('.request-id-control.compact code')!;
          const diagnostics = [...document.querySelectorAll<HTMLElement>('.request-diagnostics')].map((element) => ({
            clientWidth: element.clientWidth,
            scrollWidth: element.scrollWidth,
          }));
          return {
            documentClientWidth: document.documentElement.clientWidth,
            documentScrollWidth: document.documentElement.scrollWidth,
            tableClientWidth: tableScroller.clientWidth,
            tableScrollWidth: tableScroller.scrollWidth,
            compactIdClientWidth: compactId.clientWidth,
            compactIdScrollWidth: compactId.scrollWidth,
            diagnostics,
          };
        });
        assert.ok(layout.documentScrollWidth <= layout.documentClientWidth, `${theme} ${width}px fixture must not create page overflow`);
        assert.ok(layout.tableScrollWidth >= layout.tableClientWidth, `${theme} ${width}px table remains in its own scroll container`);
        assert.ok(layout.compactIdScrollWidth >= layout.compactIdClientWidth, `${theme} ${width}px request ID remains safely clipped in its cell`);
        assert.equal(layout.diagnostics.length, 2, `${theme} ${width}px fixture must retain both recorded and historical diagnostic surfaces`);
        for (const diagnostics of layout.diagnostics) assert.ok(diagnostics.scrollWidth <= diagnostics.clientWidth, `${theme} ${width}px each detail diagnostics surface must remain contained`);
      }
    }
  } finally {
    await context.close();
    await browser.close();
    await server.close();
  }
});
