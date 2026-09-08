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
  try {
    const page = await browser.newPage();
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/request-diagnostics.html`);
    await page.locator('.request-diagnostics').waitFor();

    const tableId = page.locator('.request-id-control.compact code');
    assert.equal(await tableId.getAttribute('title'), requestId);
    assert.equal(await tableId.textContent(), requestId);
    assert.match(await page.locator('.request-token-cell small').textContent() ?? '', /Cache read 40.*Cache write 20/);
    const recorded = page.locator('[data-fixture-request="recorded"]');
    assert.match(await recorded.textContent() ?? '', /http_429/);
    assert.match(await recorded.textContent() ?? '', new RegExp(upstreamId));
    assert.match(await recorded.textContent() ?? '', new RegExp(routeId));
    const historicalGap = page.locator('[data-fixture-request="historical-gap"]');
    const historicalGapText = await historicalGap.textContent() ?? '';
    assert.match(historicalGapText, /Completed at—/);
    assert.match(historicalGapText, /Final upstream ID—/);
    assert.match(historicalGapText, /Final route ID—/);
    assert.equal((await page.locator('tbody tr').nth(1).locator('td').nth(7).textContent())?.trim(), '—', 'an explicit historical null currency must not inherit the current credential currency');

    await page.evaluate(() => {
      Object.defineProperty(navigator, 'clipboard', {
        configurable: true,
        value: { writeText: (value: string) => { document.body.dataset.fixtureCopied = value; return Promise.resolve(); } },
      });
    });
    await page.locator('.request-id-control.compact .copy-control button').click();
    assert.equal(await page.locator('body').getAttribute('data-fixture-copied'), requestId);

    await page.locator('.request-session-cell .table-link').click();
    assert.equal(await page.locator('[data-fixture-session-opened]').textContent(), sessionId);

    for (const theme of ['dark', 'light'] as const) {
      await page.evaluate((value) => { document.documentElement.dataset.theme = value; }, theme);
      for (const width of [320, 390, 768]) {
        await page.setViewportSize({ width, height: 900 });
        await page.evaluate(() => new Promise<void>((resolve) => requestAnimationFrame(() => resolve())));
        const layout = await page.evaluate(() => {
          const tableScroller = document.querySelector<HTMLElement>('.table-scroll')!;
          const compactId = document.querySelector<HTMLElement>('.request-id-control.compact code')!;
          const diagnostics = document.querySelector<HTMLElement>('.request-diagnostics')!;
          return {
            documentClientWidth: document.documentElement.clientWidth,
            documentScrollWidth: document.documentElement.scrollWidth,
            tableClientWidth: tableScroller.clientWidth,
            tableScrollWidth: tableScroller.scrollWidth,
            compactIdClientWidth: compactId.clientWidth,
            compactIdScrollWidth: compactId.scrollWidth,
            diagnosticsClientWidth: diagnostics.clientWidth,
            diagnosticsScrollWidth: diagnostics.scrollWidth,
          };
        });
        assert.ok(layout.documentScrollWidth <= layout.documentClientWidth, `${theme} ${width}px fixture must not create page overflow`);
        assert.ok(layout.tableScrollWidth >= layout.tableClientWidth, `${theme} ${width}px table remains in its own scroll container`);
        assert.ok(layout.compactIdScrollWidth >= layout.compactIdClientWidth, `${theme} ${width}px request ID remains safely clipped in its cell`);
        assert.ok(layout.diagnosticsScrollWidth <= layout.diagnosticsClientWidth, `${theme} ${width}px detail diagnostics must remain contained`);
      }
    }
  } finally {
    await browser.close();
    await server.close();
  }
});
