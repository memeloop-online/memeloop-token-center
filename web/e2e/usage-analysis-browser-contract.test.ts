import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { mkdir, readdir, stat } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

import { chromium } from 'playwright';
import { createServer } from 'vite';

const webRoot = fileURLToPath(new URL('..', import.meta.url));
const artifactRoot = join(webRoot, 'e2e-artifacts', 'usage-analysis');
const viewports = [320, 390, 768, 1024, 1440, 1920, 2560] as const;

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

test('Usage analysis keeps exact localized metrics and real trend charts contained at product breakpoints', { timeout: 45_000 }, async () => {
  const executablePath = await localChromiumExecutable();
  if (!executablePath) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required for the usage analysis browser gate');
    return test.skip('a local Chromium runtime is required for UsageAnalysis layout assertions');
  }
  const server = await createServer({ root: webRoot, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0, strictPort: false } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ executablePath, headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
    await mkdir(artifactRoot, { recursive: true });
    for (const locale of ['en', 'zh-CN'] as const) {
      await page.addInitScript((value) => localStorage.setItem('mtc-locale', value), locale);
      await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/usage-analysis.html`);
      await page.locator('.usage-overview-chart-grid .usage-echart canvas').nth(2).waitFor();
      assert.equal(await page.locator('.usage-metrics svg').count(), 0, 'one real bucket must not fabricate a trend curve');
      assert.equal(await page.locator('.usage-metrics [data-ratio]').count(), 2, 'success and cache rates have data-backed backgrounds even without a trend');
      const cacheRate = page.locator('.usage-metrics .analytics-metric').filter({ hasText: locale === 'en' ? 'Cache rate' : '缓存率' });
      assert.equal(await cacheRate.count(), 1);
      assert.equal(await cacheRate.locator('.metric-value').innerText(), '44.44%');
      assert.equal(await cacheRate.locator('.analytics-metric-ratio').getAttribute('data-ratio'), String(4 / 9));
      for (const theme of ['dark', 'light'] as const) {
        await page.evaluate((value) => { document.documentElement.dataset.theme = value; }, theme);
        for (const width of viewports) {
          await page.setViewportSize({ width, height: 900 });
          await nextPaint(page);
          const layout = await page.evaluate(() => {
            const chartGrid = document.querySelector<HTMLElement>('.usage-overview-chart-grid')!;
            const exactValues = [...document.querySelectorAll<HTMLElement>('.usage-metrics .metric-exact')].map((exact) => {
              const range = document.createRange();
              range.selectNodeContents(exact);
              return { lines: range.getClientRects().length, text: exact.textContent, title: exact.getAttribute('title') };
            });
            return {
              documentClientWidth: document.documentElement.clientWidth,
              documentScrollWidth: document.documentElement.scrollWidth,
              columns: getComputedStyle(chartGrid).gridTemplateColumns.split(' ').length,
              charts: [...chartGrid.querySelectorAll<HTMLElement>('.usage-echart')].map((chart) => ({ clientWidth: chart.clientWidth, scrollWidth: chart.scrollWidth })),
              exactValues,
            };
          });
          assert.equal(layout.charts.length, 3, `${locale} ${theme} ${width}px renders request, latency, and cost from the usage API`);
          assert.ok(layout.documentScrollWidth <= layout.documentClientWidth, `${locale} ${theme} ${width}px does not create page-level horizontal overflow`);
          assert.ok(layout.charts.every((chart) => chart.scrollWidth <= chart.clientWidth), `${locale} ${theme} ${width}px keeps every chart in its card`);
          assert.ok(layout.exactValues.every((value) => value.lines === 1 && value.text && value.title), `${locale} ${theme} ${width}px retains each metric and its exact tooltip on one line`);
          if (width === 1440) assert.ok(layout.columns >= 3, 'wide layouts use all three overview trend cards');
          if (width <= 768) assert.equal(layout.columns, 1, 'mobile and tablet layouts stack overview charts in reading order');
          const screenshotPath = join(artifactRoot, `usage-analysis-${locale}-${theme}-${width}.png`);
          await page.screenshot({ path: screenshotPath, fullPage: true });
          assert.ok((await stat(screenshotPath)).size > 0, `${locale} ${theme} ${width}px screenshot is retained for CI inspection`);
        }
      }
    }
  } finally {
    await browser.close();
    await server.close();
  }
});

test('Usage analysis publishes completed data while the lazy chart module is paused', async () => {
  const executablePath = await localChromiumExecutable();
  if (!executablePath) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required for the usage analysis browser gate');
    return test.skip('a local Chromium runtime is required for UsageAnalysis progressive rendering assertions');
  }
  const server = await createServer({ root: webRoot, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0, strictPort: false } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ executablePath, headless: true });
  let releaseChart: () => void = () => undefined;
  const chartGate = new Promise<void>((resolve) => { releaseChart = resolve; });
  try {
    const page = await browser.newPage({ viewport: { width: 1024, height: 800 } });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.route('**/src/charts/EChart.tsx*', async (route) => {
      await chartGate;
      await route.continue();
    });
    const chartRequested = page.waitForRequest((request) => new URL(request.url()).pathname.endsWith('/src/charts/EChart.tsx'));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/usage-analysis.html`, { waitUntil: 'domcontentloaded' });
    await chartRequested;

    await page.locator('.usage-metrics').waitFor();
    assert.equal(await page.locator('.usage-echart canvas').count(), 0, 'paused chart code must not fabricate a ready chart');
    await page.getByRole('tab', { name: 'Dimensions', exact: true }).click();
    await page.locator('.usage-dimension-picker').waitFor();
    const callsWhilePaused = await page.evaluate(() => (window as unknown as { usageAnalysisFixture: { calls: string[] } }).usageAnalysisFixture.calls);
    assert.equal(callsWhilePaused.filter((path) => path.startsWith('/internal/v1/usage-analysis?')).length, 1, 'progressive rendering must not repeat the usage read');

    await page.getByRole('tab', { name: 'Overview', exact: true }).click();
    releaseChart();
    await page.locator('.usage-overview-chart-grid .usage-echart canvas').first().waitFor();
  } finally {
    releaseChart();
    await browser.close();
    await server.close();
  }
});

test('usage filter period, credential, saved AST, drilldown, and clear stay synchronized', { timeout: 45_000 }, async () => {
  const executablePath = await localChromiumExecutable();
  if (!executablePath) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required for usage filter assertions');
    return test.skip('a local Chromium runtime is required for usage filter assertions');
  }
  const server = await createServer({ root: webRoot, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0, strictPort: false } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ executablePath, headless: true });
  try {
    const page = await browser.newPage();
    const runtimeErrors: string[] = [];
    page.on('pageerror', (error) => runtimeErrors.push(error.message));
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/usage-analysis.html`);
    await page.locator('.usage-metrics').waitFor();
    const filter = page.getByRole('button', { name: 'Filter', exact: true });
    await filter.click();
    await nextPaint(page);
    assert.equal(runtimeErrors.length, 0, `filter render errors: ${runtimeErrors.join('; ')}`);
    assert.equal(await page.locator('.typed-filter-actions button').count(), 1, `filter actions remain mounted at ${page.url()}`);
    assert.equal(await filter.getAttribute('aria-expanded'), 'true', 'filter remains expanded after opening');
    const dialog = page.getByRole('dialog');
    const panelMarkup = await page.locator('.typed-filter-dialog').evaluate((panel) => ({ html: panel.innerHTML, open: panel.matches(':popover-open') }));
    assert.equal(panelMarkup.open, true, 'the filter popover opens');
    assert.match(panelMarkup.html, /Last 7 days/, 'the unified period controls render in the filter popover');
    await dialog.getByRole('button', { name: 'Last 7 days' }).click();
    await dialog.getByLabel('Client credential').selectOption('client-a');
    await dialog.getByRole('button', { name: 'Add condition' }).click();
    await dialog.locator('.typed-filter-row select').first().selectOption('protocol');
    await dialog.locator('.typed-filter-row [data-filter-field="protocol"] select').selectOption('anthropic');
    await dialog.getByPlaceholder('Filter name').fill('week-client');
    await dialog.getByRole('button', { name: 'Save filter' }).click();
    await dialog.getByRole('button', { name: 'Apply filters' }).click();
    const calls = () => page.evaluate(() => (window as unknown as { usageAnalysisFixture: { calls: string[] } }).usageAnalysisFixture.calls.filter((path) => path.startsWith('/internal/v1/usage-analysis?')));
    await page.waitForFunction(() => (window as unknown as { usageAnalysisFixture: { calls: string[] } }).usageAnalysisFixture.calls.filter((path) => path.includes('/usage-analysis?')).length >= 2);
    const presetQuery = new URLSearchParams(new URL((await calls()).at(-1)!, 'http://fixture').search);
    assert.equal(presetQuery.get('key_alias'), 'client-a');
    assert.equal(presetQuery.get('protocol'), 'anthropic');
    assert.equal(Number(presetQuery.get('to_created_at')) - Number(presetQuery.get('from_created_at')), 7 * 86_400_000);
    const saved = await page.evaluate(() => window.usageAnalysisFixture.presets.named);
    assert.equal((saved[0].ast as { conditions: Array<{ field: string }> }).conditions.filter((condition) => condition.field === 'created_at').length, 1);
    await filter.click();
    await dialog.getByRole('button', { name: 'Custom' }).click();
    await dialog.locator('.usage-custom-range input').first().fill('2026-09-01T00:00');
    await dialog.locator('.usage-custom-range input').last().fill('2026-09-02T00:00');
    await dialog.getByRole('button', { name: 'Apply filters' }).click();
    await page.waitForFunction(() => (window as unknown as { usageAnalysisFixture: { calls: string[] } }).usageAnalysisFixture.calls.filter((path) => path.includes('/usage-analysis?')).length >= 3);
    await filter.click();
    await dialog.getByRole('button', { name: 'week-client' }).click();
    assert.equal(await dialog.getByRole('button', { name: 'Custom' }).getAttribute('aria-pressed'), 'true');
    assert.equal(await dialog.getByLabel('Client credential').inputValue(), 'client-a');
    await dialog.getByRole('button', { name: 'Apply filters' }).click();
    await page.waitForFunction(() => (window as unknown as { usageAnalysisFixture: { calls: string[] } }).usageAnalysisFixture.calls.filter((path) => path.includes('/usage-analysis?')).length >= 4);
    const reopenedQuery = new URLSearchParams(new URL((await calls()).at(-1)!, 'http://fixture').search);
    assert.equal(reopenedQuery.get('from_created_at'), presetQuery.get('from_created_at'));
    assert.equal(reopenedQuery.get('to_created_at'), presetQuery.get('to_created_at'));
    assert.equal(reopenedQuery.get('protocol'), 'anthropic');
    await page.locator('.usage-overview-chart-grid .usage-chart-card').first().getByRole('tab', { name: 'Data' }).click();
    await page.locator('.usage-overview-chart-grid .usage-chart-card').first().locator('.usage-chart-table button').first().click();
    await filter.click();
    assert.equal(await dialog.getByRole('button', { name: 'Custom' }).getAttribute('aria-pressed'), 'true');
    await dialog.getByRole('button', { name: 'Clear' }).click();
    await filter.click();
    assert.equal(await dialog.getByRole('button', { name: 'Last 24 hours' }).getAttribute('aria-pressed'), 'true');
    assert.equal(await dialog.getByLabel('Client credential').inputValue(), '');
    assert.equal(await dialog.locator('.typed-filter-row').count(), 0);
  } finally {
    await browser.close();
    await server.close();
  }
});
