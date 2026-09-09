import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { mkdir, readdir } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

import { chromium, type Page } from 'playwright';
import { createServer } from 'vite';

const webRoot = fileURLToPath(new URL('..', import.meta.url));
const artifactRoot = join(webRoot, 'e2e-artifacts', 'request-diagnostics');
const stepTimeout = 5_000;
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

async function stage<T>(name: string, action: () => Promise<T>, history: string[], current: { value: string }) {
  current.value = name;
  history.push(name);
  return await new Promise<T>((resolve, reject) => {
    const timeout = setTimeout(() => reject(new Error(`timed out after ${stepTimeout}ms during ${name}`)), stepTimeout);
    void action().then((value) => { clearTimeout(timeout); resolve(value); }, (reason: unknown) => { clearTimeout(timeout); reject(reason); });
  });
}

async function fixtureDiagnostics(page: Page | undefined, phase: string, history: string[], pageErrors: string[], consoleErrors: string[]) {
  const layout = page ? await Promise.race([
    page.evaluate(() => {
    const dimensions = (selector: string) => [...document.querySelectorAll<HTMLElement>(selector)].map((element) => ({
      clientWidth: element.clientWidth,
      clientHeight: element.clientHeight,
      scrollWidth: element.scrollWidth,
      scrollHeight: element.scrollHeight,
    }));
    return {
      readyState: document.readyState,
      viewport: { width: document.documentElement.clientWidth, height: document.documentElement.clientHeight, scrollWidth: document.documentElement.scrollWidth },
      fixtureRoots: document.querySelectorAll('[data-fixture-ready="request-diagnostics"]').length,
      recordedDiagnostics: document.querySelectorAll('[data-fixture-request="recorded"] .request-diagnostics').length,
      historicalDiagnostics: document.querySelectorAll('[data-fixture-request="historical-gap"] .request-diagnostics').length,
      requestRows: document.querySelectorAll('tbody tr').length,
      copyButtons: document.querySelectorAll('.request-id-control.compact .copy-control button').length,
      tables: dimensions('.table-scroll'),
      diagnostics: dimensions('.request-diagnostics'),
    };
    }).catch((reason: unknown) => ({ evaluationError: reason instanceof Error ? reason.message : String(reason) })),
    new Promise<{ evaluationError: string }>((resolve) => setTimeout(() => resolve({ evaluationError: `layout diagnostic exceeded ${stepTimeout}ms` }), stepTimeout)),
  ]) : { page: 'not created' };
  return { phase, history, pageErrors, consoleErrors, layout };
}

test('Request diagnostics remain copyable, session-linked, and contained on narrow light and dark surfaces', { timeout: 60_000 }, async () => {
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
  const history: string[] = [];
  const current = { value: 'create browser context' };
  const pageErrors: string[] = [];
  const consoleErrors: string[] = [];
  let page: Page | undefined;
  try {
    const origin = `http://127.0.0.1:${address.port}`;
    await stage('grant clipboard permissions', () => context.grantPermissions(['clipboard-read', 'clipboard-write'], { origin }), history, current);
    page = await stage('create fixture page', () => context.newPage(), history, current);
    page.setDefaultTimeout(stepTimeout);
    page.setDefaultNavigationTimeout(stepTimeout);
    page.on('pageerror', (error) => { if (pageErrors.length < 10) pageErrors.push(error.message); });
    page.on('console', (message) => { if (message.type() === 'error' && consoleErrors.length < 10) consoleErrors.push(message.text()); });
    await stage('install fixture locale', () => page!.addInitScript(() => localStorage.setItem('mtc-locale', 'en')), history, current);
    await stage('navigate fixture', () => page!.goto(`${origin}/e2e/fixtures/request-diagnostics.html`, { waitUntil: 'domcontentloaded', timeout: stepTimeout }), history, current);
    const fixture = page.locator('[data-fixture-ready="request-diagnostics"]');
    await stage('wait fixture root', () => fixture.waitFor({ state: 'visible', timeout: stepTimeout }), history, current);
    const recorded = fixture.locator('[data-fixture-request="recorded"]');
    const historicalGap = fixture.locator('[data-fixture-request="historical-gap"]');
    const recordedDiagnostics = recorded.locator('.request-diagnostics');
    const historicalGapDiagnostics = historicalGap.locator('.request-diagnostics');
    await stage('wait recorded diagnostics', () => recordedDiagnostics.waitFor({ state: 'visible', timeout: stepTimeout }), history, current);
    await stage('wait historical diagnostics', () => historicalGapDiagnostics.waitFor({ state: 'visible', timeout: stepTimeout }), history, current);

    const recordedRow = page.locator('tbody tr').filter({ has: page.locator(`code[title="${requestId}"]`) });
    assert.equal(await stage('count recorded request row', () => recordedRow.count(), history, current), 1);
    const tableId = recordedRow.locator('.request-id-control.compact code');
    assert.equal(await stage('read table request ID title', () => tableId.getAttribute('title'), history, current), requestId);
    assert.equal(await stage('read table request ID', () => tableId.textContent(), history, current), requestId);
    assert.match(await stage('read recorded cache split', () => recordedRow.locator('.request-token-cell small').textContent(), history, current) ?? '', /Cache read 40.*Cache write 20/);
    const recordedText = await stage('read recorded diagnostics', () => recorded.textContent(), history, current) ?? '';
    assert.match(recordedText, /http_429/);
    assert.match(recordedText, new RegExp(upstreamId));
    assert.match(recordedText, new RegExp(routeId));
    const historicalGapText = await stage('read historical diagnostics', () => historicalGap.textContent(), history, current) ?? '';
    assert.match(historicalGapText, /Completed at—/);
    assert.match(historicalGapText, /Final upstream ID—/);
    assert.match(historicalGapText, /Final route ID—/);
    assert.doesNotMatch(historicalGapText, /Cache read|Cache write/, 'missing historical cache fields must remain absent rather than becoming zero-valued rows');
    assert.equal((await stage('read historical table currency', () => page!.locator('tbody tr').nth(1).locator('td').nth(7).textContent(), history, current))?.trim(), '—', 'an explicit historical null currency must not inherit the current credential currency');

    assert.equal(await stage('check Clipboard API', () => page!.evaluate(() => typeof navigator.clipboard?.writeText), history, current), 'function', 'the fixture must exercise the browser Clipboard API');
    await stage('click request copy control', () => recordedRow.locator('.request-id-control.compact .copy-control button').click({ timeout: stepTimeout }), history, current);
    await stage('wait copied copy control', () => recordedRow.getByRole('button', { name: 'Copied', exact: true }).waitFor({ timeout: stepTimeout }), history, current);
    const copiedRequestId = await stage('read browser clipboard', () => page!.evaluate(async () => new Promise<string>((resolve, reject) => {
      const timeout = window.setTimeout(() => reject(new Error('clipboard read did not settle')), 5_000);
      void navigator.clipboard.readText().then((value) => { window.clearTimeout(timeout); resolve(value); }, (reason) => { window.clearTimeout(timeout); reject(reason); });
    })), history, current);
    assert.equal(copiedRequestId, requestId);

    await stage('click request session link', () => recordedRow.locator('.request-session-cell .table-link').click({ timeout: stepTimeout }), history, current);
    assert.equal(await stage('read opened session', () => page!.locator('[data-fixture-session-opened]').textContent(), history, current), sessionId);

    for (const theme of ['dark', 'light'] as const) {
      await stage(`set ${theme} theme`, () => page!.evaluate((value) => { document.documentElement.dataset.theme = value; }, theme), history, current);
      for (const width of [320, 390, 768]) {
        await stage(`${theme} ${width}px set viewport`, () => page!.setViewportSize({ width, height: 900 }), history, current);
        await stage(`${theme} ${width}px wait paint`, () => page!.evaluate(() => new Promise<void>((resolve) => requestAnimationFrame(() => resolve()))), history, current);
        const layout = await stage(`${theme} ${width}px read layout`, () => page!.evaluate(() => {
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
        }), history, current);
        assert.ok(layout.documentScrollWidth <= layout.documentClientWidth, `${theme} ${width}px fixture must not create page overflow`);
        assert.ok(layout.tableScrollWidth >= layout.tableClientWidth, `${theme} ${width}px table remains in its own scroll container`);
        assert.ok(layout.compactIdScrollWidth >= layout.compactIdClientWidth, `${theme} ${width}px request ID remains safely clipped in its cell`);
        assert.equal(layout.diagnostics.length, 2, `${theme} ${width}px fixture must retain both recorded and historical diagnostic surfaces`);
        for (const diagnostics of layout.diagnostics) assert.ok(diagnostics.scrollWidth <= diagnostics.clientWidth, `${theme} ${width}px each detail diagnostics surface must remain contained`);
      }
    }
  } catch (reason) {
    const diagnostics = await fixtureDiagnostics(page, current.value, history, pageErrors, consoleErrors);
    process.stderr.write(`request-diagnostics browser contract failed: ${JSON.stringify(diagnostics)}\n`);
    if (page) {
      await mkdir(artifactRoot, { recursive: true });
      await page.screenshot({ path: join(artifactRoot, 'request-diagnostics-failure.png'), fullPage: true, timeout: stepTimeout }).catch((screenshotReason: unknown) => {
        process.stderr.write(`request-diagnostics failure screenshot unavailable: ${screenshotReason instanceof Error ? screenshotReason.message : String(screenshotReason)}\n`);
      });
    }
    throw reason;
  } finally {
    await context.close();
    await browser.close();
    await server.close();
  }
});
