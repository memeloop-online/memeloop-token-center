import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { mkdir, readdir, stat } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

import { chromium } from 'playwright';
import { createServer } from 'vite';

declare global {
  interface Window { upstreamAvailabilityFixture: { openedRequestId?: string } }
}

const webRoot = fileURLToPath(new URL('..', import.meta.url));
const artifactRoot = join(webRoot, 'e2e-artifacts', 'upstream-availability');
const widths = [320, 768, 1440] as const;

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

test('Upstream availability shows real routed attempts separately from the manual probe at wide and narrow widths', { timeout: 45_000 }, async () => {
  const executablePath = await localChromiumExecutable();
  if (!executablePath) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required for the upstream availability browser gate');
    return test.skip('a local Chromium runtime is required for upstream availability layout assertions');
  }
  const server = await createServer({ root: webRoot, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0, strictPort: false } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ executablePath, headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/upstream-availability.html`);
    await page.getByText('Recent availability', { exact: true }).first().waitFor();
    assert.equal(await page.locator('.provider-recent-attempts > li').count(), 5, 'the component must retain only the five latest routed terminal attempts across the account models');
    assert.equal(await page.getByText('Manual health check', { exact: true }).count(), 2, 'a manual probe is a separately labelled status, never a routed-attempt row');
    assert.equal(await page.getByText('No recent routed attempts appear in the current snapshot.', { exact: true }).count(), 1, 'an account absent from the capped snapshot is shown as unobserved rather than healthy');
    assert.equal(await page.getByText('Awaiting recovery probe', { exact: true }).count(), 2, 'breaker cooldown is presented as the current routing state for each listed model');
    assert.equal(await page.getByText('Connection unhealthy', { exact: true }).count(), 1, 'manual probe failure stays distinct from the breaker state');
    await page.locator('.provider-attempt-link').first().click();
    assert.equal(await page.evaluate(() => window.upstreamAvailabilityFixture.openedRequestId), 'req-newest', 'a routed request result delegates to the exact request-detail drilldown');

    await mkdir(artifactRoot, { recursive: true });
    for (const theme of ['dark', 'light'] as const) {
      await page.evaluate((value) => { document.documentElement.dataset.theme = value; }, theme);
      for (const width of widths) {
        await page.setViewportSize({ width, height: 900 });
        await nextPaint(page);
        const layout = await page.evaluate(() => ({
          clientWidth: document.documentElement.clientWidth,
          scrollWidth: document.documentElement.scrollWidth,
          cards: [...document.querySelectorAll<HTMLElement>('.provider-availability')].map((element) => ({ clientWidth: element.clientWidth, scrollWidth: element.scrollWidth })),
        }));
        assert.equal(layout.cards.length, 2, `${theme} ${width}px retains observed and unobserved upstream accounts`);
        assert.ok(layout.scrollWidth <= layout.clientWidth, `${theme} ${width}px must not create page-level horizontal overflow`);
        for (const card of layout.cards) assert.ok(card.scrollWidth <= card.clientWidth, `${theme} ${width}px availability card must reflow instead of overflow`);
        const screenshotPath = join(artifactRoot, `upstream-availability-${theme}-${width}.png`);
        await page.screenshot({ path: screenshotPath, fullPage: true });
        assert.ok((await stat(screenshotPath)).size > 0, `${theme} ${width}px screenshot must be saved for the CI artifact`);
      }
    }
  } finally {
    await browser.close();
    await server.close();
  }
});
