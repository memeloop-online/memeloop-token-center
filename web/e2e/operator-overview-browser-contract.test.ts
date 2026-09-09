import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { mkdir, readdir, stat } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

import { chromium } from 'playwright';
import { createServer } from 'vite';

declare global {
  interface Window {
    overviewFixture: {
      calls: string[];
      delayedTenantResponsePending: boolean;
      releaseDelayedTenantResponse: () => void;
    };
  }
}

const webRoot = fileURLToPath(new URL('..', import.meta.url));
const artifactRoot = join(webRoot, 'e2e-artifacts', 'overview-trends');
const screenshotWidths = [320, 390, 768, 1440, 2560] as const;

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

async function nextPaint(page: import('playwright').Page) {
  await page.evaluate(() => new Promise<void>((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
}

test('Overview keeps current sections visible through independent endpoint failure, late tenant data, and responsive trend charts', { timeout: 45_000 }, async () => {
  const executablePath = await localChromiumExecutable();
  if (!executablePath) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required for the Overview browser gate');
    return test.skip('a local Chromium runtime is required for Overview layout assertions');
  }
  const server = await createServer({ root: webRoot, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0, strictPort: false } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ executablePath, headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/operator-overview.html`);

    await page.getByRole('alert').getByText('Monitoring fixture unavailable', { exact: true }).waitFor();
    await page.locator('.overview-trend-card .usage-echart canvas').nth(1).waitFor();
    assert.equal(await page.locator('.operator-overview-shortcuts').count(), 0, 'overview should prioritize monitoring instead of repeating sidebar destinations');
    assert.equal(await page.locator('.overview-trend-card .usage-echart').count(), 2, 'the successful statistics endpoint must render both independent trend charts despite monitoring failure');
    assert.equal(await page.evaluate(() => window.overviewFixture.delayedTenantResponsePending), true, 'the alpha request response must still be pending before the scope change');

    const alphaCalls = await page.evaluate(() => window.overviewFixture.calls);
    for (const endpoint of ['/internal/v1/requests', '/internal/v1/monitoring-snapshot', '/internal/v1/usage-analysis']) {
      assert.ok(alphaCalls.some((call) => call.includes(endpoint) && call.includes('tenant_external_id=tenant-alpha')), `missing alpha request for ${endpoint}`);
    }

    await page.getByRole('button', { name: 'Switch tenant', exact: true }).click();
    await page.getByText('beta-current-model', { exact: true }).waitFor();
    await page.locator('.operator-monitoring').waitFor();
    await page.locator('.overview-trend-card .usage-echart canvas').nth(1).waitFor();

    await page.evaluate(() => window.overviewFixture.releaseDelayedTenantResponse());
    await page.waitForTimeout(50);
    assert.equal(await page.getByText('alpha-stale-model', { exact: true }).count(), 0, 'a late response from the previous tenant must not replace beta data');
    assert.equal(await page.getByText('beta-current-model', { exact: true }).count(), 1);

    const betaCalls = await page.evaluate(() => window.overviewFixture.calls);
    for (const endpoint of ['/internal/v1/requests', '/internal/v1/monitoring-snapshot', '/internal/v1/usage-analysis']) {
      assert.ok(betaCalls.some((call) => call.includes(endpoint) && call.includes('tenant_external_id=tenant-beta')), `missing beta request for ${endpoint}`);
    }

    const trendData = page.locator('.overview-trend-data');
    assert.equal(await trendData.getAttribute('open'), null, 'the exact UTC table starts collapsed');
    await trendData.locator('summary').click();
    await trendData.locator('tbody tr').nth(2).waitFor();
    assert.match(await trendData.locator('thead').textContent() ?? '', /UTC/);
    assert.equal(await trendData.locator('tbody tr').count(), 3, 'the expanded table exposes the exact returned points, not derived rows');

    await mkdir(artifactRoot, { recursive: true });
    for (const theme of ['dark', 'light'] as const) {
      await page.evaluate((value) => { document.documentElement.dataset.theme = value; }, theme);
      for (const width of screenshotWidths) {
        await page.setViewportSize({ width, height: 900 });
        await nextPaint(page);
        const layout = await page.evaluate(() => ({
          documentClientWidth: document.documentElement.clientWidth,
          documentScrollWidth: document.documentElement.scrollWidth,
          metricColumns: getComputedStyle(document.querySelector('.monitoring-metrics-grid')!).gridTemplateColumns.split(' ').length,
          charts: [...document.querySelectorAll<HTMLElement>('.overview-trend-card .usage-echart')].map((element) => ({ clientWidth: element.clientWidth, scrollWidth: element.scrollWidth })),
          tables: [...document.querySelectorAll<HTMLElement>('.table-scroll')].map((element) => ({ clientWidth: element.clientWidth, scrollWidth: element.scrollWidth })),
        }));
        assert.equal(layout.charts.length, 2, `${theme} ${width}px retains both trend charts`);
        assert.ok(layout.documentScrollWidth <= layout.documentClientWidth, `${theme} ${width}px must not create page-level horizontal overflow`);
        if (width >= 1440) assert.equal(layout.metricColumns, 4, 'eight summary metrics form two balanced rows on wide screens');
        for (const chart of layout.charts) assert.ok(chart.scrollWidth <= chart.clientWidth, `${theme} ${width}px charts must remain contained`);
        for (const table of layout.tables) assert.ok(table.scrollWidth >= table.clientWidth, `${theme} ${width}px tables retain their own scroll container`);
        if (width <= 768) assert.ok(layout.tables.some((table) => table.scrollWidth > table.clientWidth), `${theme} ${width}px wide request data must scroll within its table instead of overflowing the page`);

        const screenshotPath = join(artifactRoot, `overview-trends-${theme}-${width}.png`);
        await page.screenshot({ path: screenshotPath, fullPage: true });
        assert.ok((await stat(screenshotPath)).size > 0, `${theme} ${width}px screenshot must be saved for the CI artifact`);
      }
    }
  } finally {
    await browser.close();
    await server.close();
  }
});
