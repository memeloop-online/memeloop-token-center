import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { mkdir, readdir } from 'node:fs/promises';
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
      openOverview: () => void;
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

test('Overview request actions open focused details and bucket drilldowns preserve the tenant-scoped filter', { timeout: 30_000 }, async () => {
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
    await page.getByRole('button', { name: 'Open details for drilled-model', exact: true }).click();
    await page.getByRole('dialog', { name: 'drilled-model', exact: true }).waitFor();
    assert.equal(await page.evaluate(() => window.overviewDrilldownFixture.route), 'requests', 'the Overview row action uses the operator request route');
    assert.equal((await page.evaluate(() => window.overviewDrilldownFixture.calls)).some((call) => call.includes(`/internal/v1/requests/dddddddd-dddd-4ddd-8ddd-dddddddddddd?tenant_external_id=drilldown-tenant`)), true, 'requestFocus opens the selected tenant-scoped request drawer');
    await page.getByRole('button', { name: 'Close', exact: true }).click();
    await page.evaluate(() => window.overviewDrilldownFixture.openOverview());
    const firstChart = page.locator('.overview-trend-card').first();
    const trendData = firstChart.locator('.overview-trend-data');
    await firstChart.getByRole('tab', { name: 'Data', exact: true }).click();
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

test('Usage virtual credential buckets open scoped requests while real sessions retain body navigation', { timeout: 45_000 }, async () => {
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
    const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
    const pageErrors: string[] = [];
    page.on('pageerror', error => pageErrors.push(error.message));
    page.on('console', message => {
      if (message.type() === 'error' && message.text().includes('same key')) pageErrors.push(message.text());
    });
    const readyDetail = async () => {
      await page.locator('.session-detail-region .session-detail').waitFor();
      await page.getByRole('button', { name: 'Rename', exact: true }).waitFor();
      await page.getByText('The checklist is ready for review.', { exact: true }).waitFor();
      assert.equal(await page.locator('.session-title-action').count(), 1, 'one title editor per selected session');
      assert.deepEqual(pageErrors, [], 'session focus must not cause browser render errors');
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth), true, 'session detail fits the viewport');
    };
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/operator-overview-drilldown.html?usage-sessions=1`);
    await page.getByRole('tab', { name: 'Dimensions', exact: true }).click();
    await page.getByRole('button', { name: 'Sessions', exact: true }).click();
    await page.getByRole('button', { name: /Unlinked requests · Codex credential/ }).click();
    await page.waitForFunction(() => window.overviewDrilldownFixture.requestQueryBodies.length > 0);
    assert.equal(await page.evaluate(() => window.overviewDrilldownFixture.route), 'requests');
    assert.deepEqual((await page.evaluate(() => window.overviewDrilldownFixture.requestQueryBodies)).at(-1), {
      tenant_external_id: 'drilldown-tenant', limit: 100, paged: true,
      ast: { logical_operator: 'and', conditions: [
        { field: 'created_at', operator: 'between', value: { type: 'timestamp', value: bucketStart }, upper: { type: 'timestamp', value: fixtureNow } },
        { field: 'key_id', operator: 'equals', value: { type: 'uuid', value: '5dae9b5a-6bd0-54a7-ae14-fb5aa4f61650' } },
        { field: 'session_association', operator: 'equals', value: { type: 'text', value: 'unlinked' } },
      ] },
    });
    const calls = await page.evaluate(() => window.overviewDrilldownFixture.calls);
    assert.equal(calls.some(call => call.includes('/sessions/')), false, 'a virtual bucket never requests session body or context');
    await page.getByText('drilled-model', { exact: true }).waitFor();
    const evidence = join(webRoot, 'e2e-artifacts', 'usage-analysis', 'session-navigation');
    await mkdir(evidence, { recursive: true });
    await page.screenshot({ path: join(evidence, 'credential-requests.png'), fullPage: true });
    await page.evaluate(() => window.overviewDrilldownFixture.openOverview());
    await page.getByRole('tab', { name: 'Dimensions', exact: true }).click();
    await page.getByRole('button', { name: 'Sessions', exact: true }).click();
    await page.setViewportSize({ width: 390, height: 844 });
    await page.screenshot({ path: join(evidence, 'usage-buckets-mobile.png'), fullPage: true });
    await page.getByRole('button', { name: 'Named Codex conversation', exact: true }).click();
    await page.waitForFunction(() => window.overviewDrilldownFixture.calls.some(call => call.includes('/sessions/00000000-0000-4000-8000-000000000002?')));
    assert.equal(await page.evaluate(() => window.overviewDrilldownFixture.route), 'sessions');
    const focusedCalls = await page.evaluate(() => window.overviewDrilldownFixture.calls);
    assert.equal(focusedCalls.some(call => call === 'POST /internal/v1/sessions/summaries'), true, 'known identity uses the summary endpoint');
    assert.equal(focusedCalls.some(call => call.includes('/sessions?') && call.includes('q=')), false, 'exact focus never waits for a name-search list');
    await readyDetail();
    await page.screenshot({ path: join(evidence, 'real-session-detail-mobile.png'), fullPage: true });
    await page.setViewportSize({ width: 1440, height: 900 });
    await readyDetail();
    await page.screenshot({ path: join(evidence, 'real-session-detail-desktop.png'), fullPage: true });

    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/operator-overview-drilldown.html?initial-sessions=1`);
    await page.waitForFunction(() => window.overviewDrilldownFixture.calls.some(call => call.includes('/sessions/00000000-0000-4000-8000-000000000002?')));
    assert.equal(await page.evaluate(() => window.overviewDrilldownFixture.route), 'sessions', 'an aggregate first row must not navigate automatically');
    assert.equal(await page.evaluate(() => window.overviewDrilldownFixture.requestQueryBodies.length), 0);
    await page.getByRole('button', { name: 'Open Unlinked requests', exact: true }).click();
    await page.waitForFunction(() => window.overviewDrilldownFixture.requestQueryBodies.length > 0);
    assert.equal(await page.evaluate(() => window.overviewDrilldownFixture.route), 'requests', 'explicit aggregate selection navigates');
    assert.equal((await page.evaluate(() => window.overviewDrilldownFixture.calls)).some(call => /\/sessions\/unlinked(?::|%3A)/.test(call)), false);

    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/operator-overview-drilldown.html?request-session=1`);
    await page.locator('.request-table').getByRole('button', { name: 'Named Codex conversation', exact: true }).click();
    await readyDetail();
    const requestFocusCalls = await page.evaluate(() => window.overviewDrilldownFixture.calls);
    assert.equal(requestFocusCalls.includes('POST /internal/v1/sessions/summaries'), true, 'request rows carry their credential into exact summary focus');
    assert.equal(requestFocusCalls.some(call => call.includes('/sessions?') && call.includes('q=')), false);
    assert.equal(requestFocusCalls.some(call => call.includes('/sessions/00000000-0000-4000-8000-000000000002?') && call.includes('key_id=5dae9b5a-6bd0-54a7-ae14-fb5aa4f61650')), true);
    await page.screenshot({ path: join(evidence, 'request-session-detail-desktop.png'), fullPage: true });

  } finally {
    await browser.close();
    await server.close();
  }
});
