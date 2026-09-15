import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { mkdir, readdir, stat } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

import { chromium } from 'playwright';
import { createServer } from 'vite';
import { fixtureAssets } from './support/fixture-assets.js';
import { metricArea } from '../src/operator/analyticsPresentation.js';
import { dataThemes } from '../src/design-system/dataTheme.js';

declare global {
  interface Window {
    overviewFixture: {
      calls: string[];
      delayedTenantResponsePending: boolean;
      delayedTenantResponseReleased: boolean;
      drilldowns: unknown[];
      releaseDelayedTenantResponse: () => void;
    };
  }
}

const webRoot = fileURLToPath(new URL('..', import.meta.url));
const artifactRoot = join(webRoot, 'e2e-artifacts', 'overview-trends');
const screenshotWidths = [320, 390, 768, 1024, 1440, 1920, 2560] as const;
const fixtureNow = Date.UTC(2026, 8, 8, 12, 0, 0);

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

async function endpointCounts(page: import('playwright').Page, tenant: string) {
  return page.evaluate((tenantScope) => {
    const counts: Record<string, number> = {};
    for (const call of window.overviewFixture.calls) {
      const url = new URL(call.slice('GET '.length), location.origin);
      if (url.searchParams.get('tenant_external_id') !== tenantScope) continue;
      counts[url.pathname] = (counts[url.pathname] ?? 0) + 1;
    }
    return counts;
  }, tenant);
}

test('Overview keeps current sections visible through independent endpoint failure, late tenant data, and responsive trend charts', { timeout: 45_000 }, async () => {
  const executablePath = await localChromiumExecutable();
  if (!executablePath) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required for the Overview browser gate');
    return test.skip('a local Chromium runtime is required for Overview layout assertions');
  }
  const server = await createServer({ root: webRoot, configFile: false, plugins: [fixtureAssets()], logLevel: 'silent', server: { host: '127.0.0.1', port: 0, strictPort: false } });
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
    await page.locator('.overview-trend-card .usage-echart canvas').nth(2).waitFor();
    assert.equal(await page.locator('.operator-overview-shortcuts').count(), 0, 'overview should prioritize monitoring instead of repeating sidebar destinations');
    assert.equal(await page.locator('.overview-trend-card .usage-echart').count(), 3, 'the successful statistics endpoint must render request, latency, and cost trends despite monitoring failure');
    assert.equal(await page.evaluate(() => window.overviewFixture.delayedTenantResponsePending), true, 'the alpha request response must still be pending before the scope change');

    assert.deepEqual(await endpointCounts(page, 'tenant-alpha'), {
      '/internal/v1/monitoring-snapshot': 1,
      '/internal/v1/requests': 1,
      '/internal/v1/usage-analysis/trends': 1,
    }, 'alpha issues exactly one independent request per Overview resource and never waits on the complete analysis endpoint');

    await page.getByRole('button', { name: 'Switch tenant', exact: true }).click();
    await page.getByText('beta-current-model', { exact: true }).waitFor();
    await page.locator('.operator-monitoring').waitFor();
    await page.locator('.overview-trend-card .usage-echart canvas').nth(1).waitFor();
    await page.locator('.overview-trend-card .usage-echart canvas').nth(2).waitFor();

    await page.evaluate(() => window.overviewFixture.releaseDelayedTenantResponse());
    await page.waitForFunction(() => window.overviewFixture.delayedTenantResponseReleased);
    await nextPaint(page);
    assert.equal(await page.getByText('alpha-stale-model', { exact: true }).count(), 0, 'a late response from the previous tenant must not replace beta data');
    assert.equal(await page.getByText('beta-current-model', { exact: true }).count(), 1);
    const requestMetric = page.locator('.operator-monitoring-metrics .analytics-metric').first();
    assert.equal(await requestMetric.locator('svg path').getAttribute('d'), metricArea([3, 5, 9]), 'background area must use the current tenant’s actual trend points');
    assert.equal(await requestMetric.evaluate((element) => getComputedStyle(element).borderTopWidth), '0px', 'statistics use flat surfaces rather than nested card borders');
    assert.equal(await page.locator('.operator-monitoring-metrics [data-ratio]').getAttribute('data-ratio'), String(15 / 17), 'success-rate background uses actual summary counts');

    assert.deepEqual(await endpointCounts(page, 'tenant-beta'), {
      '/internal/v1/monitoring-snapshot': 1,
      '/internal/v1/requests': 1,
      '/internal/v1/usage-analysis/trends': 1,
    }, 'beta renders its trends from one projection request and never calls the complete analysis endpoint configured to return 500');

    const firstChart = page.locator('.overview-trend-card').first();
    const trendData = firstChart.locator('.overview-trend-data');
    assert.equal(await trendData.isVisible(), false, 'the chart is the initial view');
    assert.equal(await page.locator('.overview-trends details, .overview-trends summary').count(), 0);
    await firstChart.getByRole('tab', { name: 'Data', exact: true }).click();
    await trendData.locator('tbody tr').nth(2).waitFor();
    assert.match(await trendData.locator('thead').textContent() ?? '', /UTC/);
    assert.equal(await trendData.getByRole('columnheader', { name: 'Total cost', exact: true }).count(), 1);
    assert.equal(await trendData.locator('tbody tr').count(), 3, 'the data view exposes the exact returned points, not derived rows');
    assert.deepEqual(await endpointCounts(page, 'tenant-beta'), { '/internal/v1/monitoring-snapshot': 1, '/internal/v1/requests': 1, '/internal/v1/usage-analysis/trends': 1 }, 'view switching does not re-fetch data');
    await trendData.locator('tbody button').nth(1).click();
    assert.deepEqual(await page.evaluate(() => window.overviewFixture.drilldowns.at(-1)), {
      logical_operator: 'and',
      conditions: [{
        field: 'created_at', operator: 'between', value: { type: 'timestamp', value: fixtureNow - 3_600_000 },
        upper: { type: 'timestamp', value: fixtureNow - 1 },
      }],
    }, 'the accessible time-bucket control uses the exact API bucket window for its request drilldown');

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
        assert.equal(layout.charts.length, 3, `${theme} ${width}px retains request, latency, and cost trend charts`);
        assert.ok(layout.documentScrollWidth <= layout.documentClientWidth, `${theme} ${width}px must not create page-level horizontal overflow`);
        if (width >= 1440) assert.equal(layout.metricColumns, 4, 'eight summary metrics form two balanced rows on wide screens');
        for (const chart of layout.charts) assert.ok(chart.scrollWidth <= chart.clientWidth, `${theme} ${width}px charts must remain contained`);
        for (const table of layout.tables) assert.ok(table.scrollWidth >= table.clientWidth, `${theme} ${width}px tables retain their own scroll container`);
        if (width <= 768) assert.ok(layout.tables.some((table) => table.scrollWidth > table.clientWidth), `${theme} ${width}px wide request data must scroll within its table instead of overflowing the page`);

        const screenshotPath = join(artifactRoot, `overview-trends-${theme}-${width}.png`);
        await page.screenshot({ path: screenshotPath, fullPage: true });
        assert.ok((await stat(screenshotPath)).size > 0, `${theme} ${width}px screenshot must be saved for the CI artifact`);
      }
      await firstChart.getByRole('tab', { name: 'Chart', exact: true }).click();
      await page.setViewportSize({ width: 1440, height: 1000 });
      for (const color of [dataThemes[theme].primary, dataThemes[theme].negative]) {
        await page.waitForFunction(hex => {
          const expected = [1, 3, 5].map(offset => parseInt(hex.slice(offset, offset + 2), 16));
          return [...document.querySelectorAll<HTMLCanvasElement>('.overview-trend-card:first-child canvas')].some(canvas => {
            if (!canvas.width || !canvas.height) return false;
            const pixels = canvas.getContext('2d')?.getImageData(0, 0, canvas.width, canvas.height).data;
            if (!pixels) return false;
            for (let index = 0; index < pixels.length; index += 4) {
              if (pixels[index] === expected[0] && pixels[index + 1] === expected[1] && pixels[index + 2] === expected[2] && pixels[index + 3] > 200) return true;
            }
            return false;
          });
        }, color, { timeout: 10_000 });
      }
      await page.screenshot({ path: join(artifactRoot, `overview-canvas-${theme}.png`), fullPage: true });
      await firstChart.getByRole('tab', { name: 'Data', exact: true }).click();
    }
  } finally {
    await browser.close();
    await server.close();
  }
});
