import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { mkdir, readdir, stat } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

import { chromium } from 'playwright';
import { createServer } from 'vite';

const webRoot = fileURLToPath(new URL('..', import.meta.url));
const artifactRoot = join(webRoot, 'e2e-artifacts', 'session-replay');
const screenshotWidths = [320, 768, 1440] as const;
const screenshotLocales = ['zh-CN', 'en'] as const;

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

test('SessionReplayPanel renders archived content, keeps missing data explicit, and saves responsive theme screenshots', { timeout: 45_000 }, async () => {
  const executablePath = await localChromiumExecutable();
  if (!executablePath) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required for the session replay browser gate');
    return test.skip('a local Chromium runtime is required for session replay screenshots');
  }
  const server = await createServer({ root: webRoot, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0, strictPort: false } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ executablePath, headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/session-replay.html`);
    await page.locator('[data-fixture-ready="session-replay"] .session-replay-entry.tool-result').waitFor();
    assert.equal(await page.locator('.session-replay-entry.message.user').count(), 1, 'the fixture must render the archive user message');
    assert.equal(await page.locator('.session-replay-entry.message.assistant').count(), 2, 'the fixture must render both archive agent messages');
    assert.equal(await page.locator('.session-replay-entry.tool-call').count(), 1);
    assert.equal(await page.locator('.session-replay-entry.tool-result').count(), 1);
    assert.equal(await page.locator('.session-replay-entry.unknown').count(), 2, 'one compact unavailable entry is retained for each affected request');
    assert.equal(await page.locator('.session-replay-expand').count(), 1, 'a long message keeps a readable preview before expansion');
    const turn = page.locator('.session-replay-turns button').first();
    assert.match(await turn.getAttribute('title') ?? '', /precipitation probability/);
    await turn.click();
    assert.equal(await turn.getAttribute('aria-pressed'), 'true');

    await mkdir(artifactRoot, { recursive: true });
    for (const locale of screenshotLocales) {
      await page.evaluate((value) => localStorage.setItem('mtc-locale', value), locale);
      await page.reload();
      await page.locator('[data-fixture-ready="session-replay"] .session-replay-entry.tool-result').waitFor();
      for (const theme of ['dark', 'light'] as const) {
        await page.evaluate((value) => { document.documentElement.dataset.theme = value; }, theme);
        for (const width of screenshotWidths) {
          await page.setViewportSize({ width, height: 900 });
          await nextPaint(page);
          const layout = await page.evaluate(() => {
            const replay = document.querySelector<HTMLElement>('.session-replay')!;
            const feed = document.querySelector<HTMLElement>('.session-replay-feed')!;
            const turns = document.querySelector<HTMLElement>('.session-replay-turns')!;
            return {
              documentClientWidth: document.documentElement.clientWidth,
              documentScrollWidth: document.documentElement.scrollWidth,
              replayClientWidth: replay.clientWidth,
              replayScrollWidth: replay.scrollWidth,
              feedClientWidth: feed.clientWidth,
              feedScrollWidth: feed.scrollWidth,
              turnsClientWidth: turns.clientWidth,
              turnsScrollWidth: turns.scrollWidth,
            };
          });
          assert.ok(layout.documentScrollWidth <= layout.documentClientWidth, `${locale} ${theme} ${width}px must not create document overflow`);
          assert.ok(layout.replayScrollWidth <= layout.replayClientWidth, `${locale} ${theme} ${width}px replay surface must remain contained`);
          assert.ok(layout.feedScrollWidth <= layout.feedClientWidth, `${locale} ${theme} ${width}px replay feed must remain contained`);
          assert.ok(layout.turnsScrollWidth <= layout.turnsClientWidth || width <= 560, `${locale} ${theme} ${width}px user-turn navigation may only scroll internally on narrow screens`);
          const screenshotPath = join(artifactRoot, `session-replay-${locale}-${theme}-${width}.png`);
          await page.screenshot({ path: screenshotPath, fullPage: true });
          assert.ok((await stat(screenshotPath)).size > 0, `${locale} ${theme} ${width}px screenshot must be saved for the CI artifact`);
        }
      }
    }
  } finally {
    await browser.close();
    await server.close();
  }
});
