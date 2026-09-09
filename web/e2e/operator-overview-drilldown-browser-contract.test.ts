import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { readdir } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

import { chromium } from 'playwright';
import { createServer } from 'vite';

declare global {
  interface Window {
    overviewDrilldownFixture: {
      calls: string[];
      requestQueryBodies: unknown[];
      route: string;
    };
  }
}

const webRoot = fileURLToPath(new URL('..', import.meta.url));
const fixtureNow = Date.UTC(2026, 8, 8, 12, 0, 0);
const bucketStart = fixtureNow - 3_600_000;

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

test('Overview bucket drilldown navigates to Requests with the exact tenant-scoped filter', { timeout: 30_000 }, async () => {
  const executablePath = await localChromiumExecutable();
  if (!executablePath) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required for the Overview drilldown browser gate');
    return test.skip('a local Chromium runtime is required for Overview drilldown assertions');
  }
  const server = await createServer({ root: webRoot, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0, strictPort: false } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ executablePath, headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/operator-overview-drilldown.html`);
    const trendData = page.locator('.overview-trend-data');
    await trendData.locator('summary').click();
    await trendData.locator('tbody button').first().click();
    await page.getByText('drilled-model', { exact: true }).waitFor();

    assert.equal(await page.evaluate(() => window.overviewDrilldownFixture.route), 'requests');
    const queryBodies = await page.evaluate(() => window.overviewDrilldownFixture.requestQueryBodies);
    assert.deepEqual(queryBodies.at(-1), {
      tenant_external_id: 'drilldown-tenant', limit: 100, paged: true,
      ast: {
        logical_operator: 'and',
        conditions: [{
          field: 'created_at', operator: 'between', value: { type: 'timestamp', value: bucketStart },
          upper: { type: 'timestamp', value: fixtureNow - 1 },
        }],
      },
    });
    assert.equal((await page.evaluate(() => window.overviewDrilldownFixture.calls)).some((call) => call.includes('/internal/v1/requests/query')), true);
  } finally {
    await browser.close();
    await server.close();
  }
});
