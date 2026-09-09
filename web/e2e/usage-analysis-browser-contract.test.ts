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
              return { lines: range.getClientRects().length, text: exact.textContent };
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
          assert.ok(layout.exactValues.every((value) => value.lines === 1 && value.text), `${locale} ${theme} ${width}px retains each exact localized metric on one line`);
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
