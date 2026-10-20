import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { mkdir, readdir, stat } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

import { chromium } from 'playwright';
import { createServer } from 'vite';

declare global { interface Window { sessionReplayReads: Record<string, number>; sessionReplayAborts: number; archiveRangeReads: number; contextReads: number; contextContentReads: number } }

const webRoot = fileURLToPath(new URL('..', import.meta.url));
const artifactRoot = join(webRoot, 'e2e-artifacts', 'session-replay');
const screenshotWidths = [320, 768, 1440] as const;
const screenshotLocales = ['zh-CN', 'en'] as const;

test('prefix reader never fetches archives or tool bodies before their disclosure is opened', { timeout: 45_000 }, async () => {
  const executablePath = await localChromiumExecutable();
  if (!executablePath) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    return test.skip('Chromium required');
  }
  const server = await createServer({ root: webRoot, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ executablePath, headless: true });
  try {
    const page = await browser.newPage();
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/session-replay.html?context=1&response=1`);
    await page.getByText('Retained final answer', { exact: true }).waitFor();
    assert.deepEqual(await page.evaluate(() => window.sessionReplayReads), {});
    assert.equal(await page.evaluate(() => window.contextReads), 1);
    assert.equal(await page.evaluate(() => window.contextContentReads), 0);
    await page.getByText('Current request final answer', { exact: true }).waitFor();
    assert.deepEqual(await page.evaluate(() => window.sessionReplayReads), {}, 'final response polling must not open the raw archive');
    await page.getByRole('button', { name: 'Full conversation and execution details', exact: true }).click();
    await page.getByRole('button', { name: /Tool calls and results/ }).click();
    await page.getByRole('button', { name: 'function_call · 3', exact: true }).click();
    assert.equal(await page.evaluate(() => window.contextContentReads), 0);
    await page.getByRole('button', { name: 'Call arguments', exact: true }).click();
    await page.getByText(/deferred-tool-secret/).waitFor();
    assert.equal(await page.evaluate(() => window.contextContentReads), 1);
    assert.deepEqual(await page.evaluate(() => window.sessionReplayReads), {});
    await page.getByRole('button', { name: 'Original request and response archives', exact: true }).click();
    await page.waitForFunction(() => Object.keys(window.sessionReplayReads).length > 0);
  } finally { await browser.close(); await server.close(); }
});

test('conversation history pages request replies across resets without opening archives', { timeout: 45_000 }, async () => {
  const executablePath = await localChromiumExecutable();
  if (!executablePath) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    return test.skip('Chromium required');
  }
  const server = await createServer({ root: webRoot, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ executablePath, headless: true });
  try {
    const page = await browser.newPage();
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/session-replay.html?context=1&response=1&history=1`);
    for (let index = 2; index <= 5; index += 1) {
      await page.getByText(`Current request final answer · replay-r${index}`, { exact: true }).waitFor();
    }
    assert.equal(await page.evaluate(() => window.contextReads), 4);
    assert.equal(await page.locator('.session-context-columns aside .session-replay-entry').count(), 2, 'shared prefix users appear once, but repeated text at a new node remains visible');
    assert.equal(await page.locator('.session-context-turn').count(), 4);
    assert.equal(await page.evaluate(() => window.contextContentReads), 0);
    assert.deepEqual(await page.evaluate(() => window.sessionReplayReads), {});
    await mkdir(artifactRoot, { recursive: true });
    for (const theme of ['light', 'dark']) {
      await page.evaluate(value => { document.documentElement.dataset.theme = value; }, theme);
      for (const width of screenshotWidths) {
        await page.setViewportSize({ width, height: 900 });
        await nextPaint(page);
        const overflow = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
        assert.ok(overflow <= 1, `history overflows at ${width}px in ${theme}`);
        if (width === 1440) {
          const columns = page.locator('.session-context-columns').first();
          const assistant = await columns.locator(':scope > section').boundingBox();
          const user = await columns.locator(':scope > aside').boundingBox();
          assert.ok(assistant && user && assistant.x + assistant.width <= user.x, 'user messages stay on the right');
        }
        await page.screenshot({ path: join(artifactRoot, `prefix-history-${theme}-${width}.png`), fullPage: true });
      }
    }
    await page.getByRole('button', { name: 'Earlier turns', exact: true }).click();
    await page.getByText('Current request final answer · replay-r1', { exact: true }).waitFor();
    await page.getByText('User message before reset', { exact: true }).waitFor();
    assert.equal(await page.locator('.session-context-turn').count(), 1, 'previous window is unmounted');
    assert.equal(await page.evaluate(() => window.contextReads), 5);
    assert.equal(await page.getByRole('button', { name: 'Earlier turns', exact: true }).isDisabled(), true);
    assert.equal(await page.evaluate(() => window.contextContentReads), 0);
    assert.deepEqual(await page.evaluate(() => window.sessionReplayReads), {});
    await page.getByRole('button', { name: 'Newer turns', exact: true }).click();
    await page.getByText('Current request final answer · replay-r5', { exact: true }).waitFor();
    await page.getByRole('combobox', { name: 'History timestamp', exact: true }).selectOption('replay-r1');
    await page.getByText('User message before reset', { exact: true }).waitFor();
    assert.equal(await page.locator('.session-context-turn').count(), 0, 'individual context inspection is an explicit mode');
  } finally { await browser.close(); await server.close(); }
});

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

async function expandReplay(page: import('playwright').Page) {
  // Expand the new hierarchy without opening large archive byte readers.
  for (let depth = 0; depth < 4; depth += 1) {
    const buttons = page.locator('.session-replay button[aria-expanded="false"]').filter({
      hasText: /Full conversation and execution details|完整对话与执行详情|Tool calls and results|工具调用与结果|Tool call ·|Tool result ·|工具调用 ·|工具结果 ·/,
    });
    // Re-query the first closed disclosure after each click: nth locators from
    // all() shift when the aria-expanded=false filter loses the opened item.
    for (let count = await buttons.count(); count > 0; count -= 1) await buttons.first().click();
  }
}

test('large archive content is explicit, paged to its real end, and cleared on version or scope changes', { timeout: 45_000 }, async () => {
  const executablePath = await localChromiumExecutable();
  if (!executablePath) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    return test.skip('Chromium required');
  }
  const server = await createServer({ root: webRoot, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ executablePath, headless: true });
  try {
    const page = await browser.newPage();
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/session-replay.html?full=1`);
    const open = page.getByRole('button', { name: 'Read full Response content', exact: true });
    await open.waitFor();
    assert.equal(await page.evaluate(() => window.archiveRangeReads), 0);
    assert.deepEqual(await page.locator('.session-replay-turn-heading b').allTextContents(), ['—'], 'snapshot-only data cannot claim zero user activity');
    assert.equal(await page.getByText('This history has not loaded yet. Try again later.', { exact: true }).count(), 0, 'a readable large archive is not labelled unavailable');
    await open.click();
    const reader = page.locator('.archive-content-reader:not(.collapsed)');
    await reader.locator('.session-replay-entry.message').nth(29).waitFor();
    const firstReads = await page.evaluate(() => window.archiveRangeReads);
    await nextPaint(page);
    assert.equal(await page.evaluate(() => window.archiveRangeReads), firstReads, 'reading stops at the displayed item page');
    assert.equal(await reader.locator('.session-replay-entry.message').count(), 30);
    assert.deepEqual(await page.locator('.session-replay-turn-heading b').allTextContents(), ['—'], 'partial reading does not invent a complete archive count');
    await mkdir(artifactRoot, { recursive: true });
    for (const theme of ['light', 'dark']) {
      await page.evaluate(value => { document.documentElement.dataset.theme = value; }, theme);
      for (const width of [390, 1440]) {
        await page.setViewportSize({ width, height: 900 });
        await nextPaint(page);
        await page.screenshot({ path: join(artifactRoot, `full-archive-${theme}-${width}.png`), fullPage: true });
      }
    }
    await reader.getByRole('button', { name: 'Next content', exact: true }).click();
    await reader.getByText(/^Archived step 31:/).first().waitFor();
    assert.equal(await reader.locator('.session-replay-entry.message').count(), 30);
    await reader.getByRole('button', { name: 'Previous content', exact: true }).click();
    await reader.getByText(/^Archived step 1:/).first().waitFor();
    await page.getByRole('button', { name: 'Change archive version', exact: true }).click();
    await reader.getByRole('button', { name: 'Next content', exact: true }).click();
    await reader.getByRole('alert').waitFor();
    assert.equal(await reader.locator('.session-replay-entry.message').count(), 0, 'version changes discard old structured content');
    await reader.getByRole('button', { name: 'Retry', exact: true }).click();
    await reader.getByText(/^Archived step 1:/).first().waitFor();
    await reader.getByRole('button', { name: 'Next content', exact: true }).click();
    await reader.getByText(/^Archived step 31:/).first().waitFor();
    await reader.getByRole('button', { name: 'Next content', exact: true }).click();
    await reader.getByText(/^Archived step 75:/).first().waitFor();
    assert.equal(await reader.locator('.session-replay-entry.message').count(), 15);
    assert.equal(await reader.getByRole('button', { name: 'Next content', exact: true }).isDisabled(), true);
    await reader.getByRole('button', { name: 'Close', exact: true }).click();
    await open.waitFor();
    assert.equal(await page.locator('.archive-content-reader .session-replay-entry.message').count(), 0);
    await open.click();
    await reader.getByText(/^Archived step 1:/).first().waitFor();
    await page.getByRole('button', { name: 'Change full scope', exact: true }).click();
    await open.waitFor();
    assert.equal(await page.locator('.archive-content-reader .session-replay-entry.message').count(), 0);
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/session-replay.html?full=1&invalid=1`);
    await open.waitFor();
    assert.equal(await page.evaluate(() => window.archiveRangeReads), 0, 'invalid snapshots also require explicit reading');
    await open.click();
    await reader.getByText(/^Archived step 1:/).first().waitFor();
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/session-replay.html?full=1&invalid-payload=1`);
    await open.waitFor();
    await open.click();
    await reader.getByRole('alert').getByText('The archive format could not be read.', { exact: true }).waitFor();
    const zhPage = await browser.newPage();
    await zhPage.addInitScript(() => localStorage.setItem('mtc-locale', 'zh-CN'));
    await zhPage.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/session-replay.html?full=1&invalid-payload=1`);
    const openZh = zhPage.getByRole('button', { name: '读取完整响应内容', exact: true });
    await openZh.waitFor();
    await openZh.click();
    await zhPage.getByRole('alert').getByText('归档格式无法读取。', { exact: true }).waitFor();
    await zhPage.close();
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/session-replay.html?full=1&gap=1`);
    await page.getByText('This history has not loaded yet. Try again later.', { exact: true }).waitFor();
    assert.equal(await open.count(), 0, 'a confirmed archive gap is not offered as a readable bound object');
    assert.equal(await page.evaluate(() => window.archiveRangeReads), 0);
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/session-replay.html?full=1&missing=1`);
    await open.waitFor();
    assert.equal(await page.evaluate(() => window.archiveRangeReads), 0);
    await open.click();
    await reader.getByRole('alert').getByText('This history has not loaded yet. Try again later.', { exact: true }).waitFor();
    assert.equal(await reader.locator('.session-replay-entry.message').count(), 0, 'a truly missing bound object remains an error, never an empty successful archive');
  } finally { await browser.close(); await server.close(); }
});

test('slow replay reads survive live metadata refresh, publish incrementally and isolate scopes', { timeout: 30_000 }, async () => {
  const executablePath = await localChromiumExecutable();
  if (!executablePath) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    return test.skip('Chromium required');
  }
  const server = await createServer({ root: webRoot, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ executablePath, headless: true });
  try {
    const page = await browser.newPage();
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/session-replay.html?live=1`);
    await page.locator('.session-replay-turns button').first().waitFor();
    await expandReplay(page);
    await page.locator('.session-replay-entry.message.user').waitFor();
    assert.equal(await page.locator('.session-replay-entry.tool-result').count(), 0, 'fast content appears before the deliberately slow tool archive');
    for (let index = 4; index < 7; index += 1) {
      await page.getByRole('button', { name: 'Append live request', exact: true }).click();
      await page.waitForFunction(id => window.sessionReplayReads[id] === 1, `additional-${index}`);
    }
    assert.equal(await page.evaluate(() => window.sessionReplayReads['replay-r2']), 1, 'new live requests never abort or restart an unchanged slow read');
    assert.equal(await page.evaluate(() => window.sessionReplayAborts), 0);
    await page.getByRole('button', { name: 'Switch replay scope', exact: true }).click();
    assert.equal(await page.locator('.session-replay-entry.message').count(), 0, 'the previous scope is hidden immediately, before the new read completes');
    await page.getByRole('button', { name: 'Resume scope reads', exact: true }).click();
    await page.locator('.session-replay-turns button').first().waitFor();
    await expandReplay(page);
    await page.locator('.session-replay-entry.message.user').waitFor();
    await page.getByRole('button', { name: 'Release slow archive', exact: true }).click();
    await page.locator('.session-replay[data-replay-loading="false"]').waitFor();
    await expandReplay(page);
    await page.locator('.session-replay-entry.tool-result').waitFor();
    assert.equal(await page.evaluate(() => window.sessionReplayReads['replay-r1']), 2, 'metadata-only refreshes never restart completed or in-flight reads');
    assert.equal(await page.evaluate(() => window.sessionReplayReads['replay-r2']), 2);
    assert.ok(await page.evaluate(() => window.sessionReplayAborts) > 0, 'scope transition cancels obsolete reads');
    const originalTurn = page.locator('.session-replay-turns button').filter({ hasText: 'Find the forecast for Oslo' });
    await originalTurn.click();
    const originalEntry = page.locator('.session-replay-layout > .session-replay-feed > li').filter({ has: page.locator('.session-replay-entry.message.user') });
    await originalEntry.evaluate(element => element.setAttribute('data-retained-test', 'true'));
    await page.getByRole('button', { name: 'Append earlier archive', exact: true }).click();
    await page.waitForFunction(() => window.sessionReplayReads['earlier-late'] === 1);
    assert.equal(await originalTurn.getAttribute('aria-pressed'), 'true', 'new unrelated requests retain the selected user turn');
    await page.getByRole('button', { name: 'Release earlier archive', exact: true }).click();
    await page.locator('.session-replay-turns').getByText('Earlier restored user turn', { exact: true }).waitFor();
    await expandReplay(page);
    await page.locator('.session-replay-feed').getByText('Earlier restored user turn', { exact: true }).waitFor();
    assert.equal(await originalTurn.getAttribute('aria-pressed'), 'true', 'earlier archives do not move the selected identity to another message');
    assert.equal(await page.locator('.session-replay-feed > li[data-retained-test="true"].selected').count(), 1, 'late insertion preserves the original message DOM and reading state');
    await page.getByRole('button', { name: 'Finish late archive', exact: true }).click();
    await page.locator('.session-replay-feed').getByText('Late archive arrived', { exact: true }).first().waitFor();
    assert.equal(await page.evaluate(() => window.sessionReplayReads['replay-r1']), 2, 'complete archives are reused inside the same bounded scope');
    assert.equal(await page.evaluate(() => window.sessionReplayReads['replay-r3']), 3, 'late availability refreshes an incomplete archive');
    await page.getByRole('button', { name: 'Invalidate complete archive', exact: true }).click();
    await page.waitForFunction(() => window.sessionReplayReads['replay-r1'] === 3);
    assert.equal(await page.locator('.session-replay-entry.message.user').filter({ hasText: 'Find the forecast for Oslo' }).count(), 0, 'a changed archive revision invalidates even previously complete cached content');
    assert.equal(await page.locator('.session-replay-turns button[aria-pressed="true"]').count(), 0, 'revoking the selected archive clears its selection');
    assert.equal(await page.locator('.session-replay-entry.message.user').filter({ hasText: 'Earlier restored user turn' }).count(), 1, 'revocation does not discard unrelated archived messages');
  } finally { await browser.close(); await server.close(); }
});

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
    await page.locator('.session-replay[data-replay-loading="false"]').waitFor();
    assert.equal(await page.locator('.session-replay-entry.tool-call').count(), 0, 'tool bodies are not mounted by default');
    assert.equal(await page.locator('.session-replay-entry.tool-result').count(), 0, 'tool results are not mounted by default');
    const userText = page.locator('.session-replay-turns button b').first();
    assert.equal(await userText.evaluate(element => getComputedStyle(element).whiteSpace), 'pre-wrap', 'the full user message wraps without ellipsis');
    const sides = await page.evaluate(() => ({
      users: document.querySelector('.session-replay-turns')!.getBoundingClientRect().left,
      responses: document.querySelector('.session-replay-feed')!.getBoundingClientRect().left,
    }));
    assert.ok(sides.users > sides.responses, 'user messages appear on the right');
    await expandReplay(page);
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
      await page.locator('.session-replay[data-replay-loading="false"]').waitFor();
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
