import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium, type Route } from 'playwright';
import { createServer } from 'vite';

const key = (alias: string, generation: number) => ({
  key_id: `key-${alias.toLowerCase()}`, alias, currency: 'USD', credential_generation: generation, created_at: 1,
  available_balance: '100', policy: { enforcement_mode: 'prepaid', requests_per_minute: 60, tokens_per_minute: 6000, max_concurrency: 4, daily_budget: null, weekly_budget: null, lifetime_budget: null },
});
const stats = (key_id: string) => ({ key_id, summary: { total_requests: 51, successful_requests: 51, failed_requests: 0, input_tokens: 51, output_tokens: 51, total_cost: '0', costs: [] }, by_model: [], by_day: [], errors: [] });
const request = (request_id: string, created_at: number, model = request_id) => ({
  request_id, created_at, completed_at: created_at + 1, protocol: 'openai', model, status_code: 200, duration_ms: 1,
  input_tokens: 1, output_tokens: 1, cost: '0', error_code: null, currency: 'USD', archive_state: 'complete', usage_basis: 'provider_reported',
});
const firstPage = Array.from({ length: 50 }, (_, index) => request(`second-${String(index).padStart(2, '0')}`, 1_000 - index));

test('self request polling is accessible, identity-safe, visibility-aware, and history-stable', { timeout: 45_000 }, async () => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    return test.skip('Chromium required');
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  let heldFirstRequest: Route | undefined;
  let heldHistoryRefresh: Route | undefined;
  let listReads = 0;
  let historyRefresh = false;
  try {
    const page = await browser.newPage();
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.route('**/self/v1/**', async route => {
      const url = new URL(route.request().url());
      const authorization = route.request().headers().authorization ?? '';
      const first = authorization === 'Bearer first';
      if (url.pathname.endsWith('/key')) return route.fulfill({ json: first ? key('First credential', 1) : key('Second credential', 2) });
      if (url.pathname.endsWith('/stats')) return route.fulfill({ json: stats(first ? 'key-first credential' : 'key-second credential') });
      if (url.pathname.endsWith('/sessions')) return route.fulfill({ json: { generated_at: 1, sessions: [], next_cursor: null } });
      if (!url.pathname.endsWith('/requests')) return route.fulfill({ json: [] });
      if (first) { heldFirstRequest = route; return; }
      listReads += 1;
      if (url.searchParams.has('before_id')) return route.fulfill({ json: [request('older-second', 900)] });
      if (historyRefresh) { heldHistoryRefresh = route; return; }
      return route.fulfill({ json: firstPage });
    });
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/self-request-refresh.html`);
    const credential = page.getByLabel('Client credential', { exact: true });
    await credential.fill('first');
    await page.getByRole('button', { name: 'Continue', exact: true }).click();
    await page.getByRole('button', { name: 'Clear credential', exact: true }).click();
    await credential.fill('second');
    await page.getByRole('button', { name: 'Continue', exact: true }).click();
    await page.locator('.request-model-cell code').filter({ hasText: /^second-00$/ }).waitFor();
    if (heldFirstRequest) await heldFirstRequest.fulfill({ json: [request('old-first-response', 3_000)] }).catch(() => undefined);
    assert.equal(await page.getByText('old-first-response', { exact: true }).count(), 0, 'an aborted first credential response cannot populate the second credential page');

    const cadence = page.getByRole('slider', { name: 'Refresh cadence', exact: true });
    assert.equal(await cadence.inputValue(), '1', 'the default cadence is five seconds');
    assert.deepEqual(await page.locator('.request-refresh-ticks span').allTextContents(), ['Manual', '5s', '30s', '1m', '5m']);
    await cadence.press('Home');
    assert.equal(await cadence.inputValue(), '0');
    await page.getByRole('status').filter({ hasText: 'Manual' }).waitFor();
    await cadence.press('ArrowRight');
    assert.equal(await cadence.inputValue(), '1');

    const beforeHidden = listReads;
    const noHiddenPoll = page.waitForRequest(request => new URL(request.url()).pathname === '/self/v1/requests', { timeout: 5_200 })
      .then(() => true).catch(() => false);
    await page.evaluate(() => {
      Object.defineProperty(document, 'hidden', { configurable: true, value: true });
      document.dispatchEvent(new Event('visibilitychange'));
    });
    await page.getByRole('status').filter({ hasText: 'paused in background' }).waitFor();
    assert.equal(await noHiddenPoll, false, 'hidden pages do not poll');
    assert.equal(listReads, beforeHidden, 'hidden pages do not poll');
    const resumedPoll = page.waitForResponse(response => new URL(response.url()).pathname === '/self/v1/requests', { timeout: 7_000 });
    await page.evaluate(() => {
      Object.defineProperty(document, 'hidden', { configurable: true, value: false });
      document.dispatchEvent(new Event('visibilitychange'));
    });
    await resumedPoll;
    assert.ok(listReads > beforeHidden, 'visible pages resume polling');

    await cadence.press('Home');
    await page.getByRole('status').filter({ hasText: 'Manual' }).waitFor();
    await page.getByRole('button', { name: 'Load older requests', exact: true }).click();
    await page.locator('.request-model-cell code').filter({ hasText: /^older-second$/ }).waitFor();
    const loaded = await page.locator('.request-id-control.compact code').allTextContents();
    historyRefresh = true;
    const refreshStarted = page.waitForRequest(request => new URL(request.url()).pathname === '/self/v1/requests', { timeout: 5_000 });
    await page.getByRole('button', { name: 'Refresh', exact: true }).click();
    await refreshStarted;
    await page.getByRole('status').filter({ hasText: 'Updating the list and summary' }).waitFor();
    await heldHistoryRefresh!.fulfill({ json: [request('new-injected', 2_000), ...firstPage.slice(0, 49)] });
    await page.getByRole('status').filter({ hasText: 'Manual' }).waitFor();
    assert.deepEqual(await page.locator('.request-id-control.compact code').allTextContents(), loaded, 'refresh cannot inject or reorder an explicit history window');
    assert.equal(await page.getByText('new-injected', { exact: true }).count(), 0);

    historyRefresh = false;
    await cadence.press('ArrowRight');
    await cadence.press('ArrowRight');
    assert.equal(await cadence.inputValue(), '2', 'the third ladder position is thirty seconds');
    await page.reload();
    await page.locator('.request-model-cell code').filter({ hasText: /^second-00$/ }).waitFor();
    const restoredCadence = page.getByRole('slider', { name: 'Refresh cadence', exact: true });
    assert.equal(await restoredCadence.inputValue(), '2', 'the selected cadence is restored from storage');
    await page.getByRole('tab', { name: 'My sessions and requests', exact: true }).click();
    const sessionsCadence = page.getByRole('slider', { name: 'Refresh cadence', exact: true });
    assert.equal(await sessionsCadence.inputValue(), '2', 'Requests and Sessions share the cadence preference');
    await page.getByRole('tab', { name: 'Recent requests', exact: true }).click();
    assert.equal(await page.getByRole('slider', { name: 'Refresh cadence', exact: true }).inputValue(), '2');
  } finally { await browser.close(); await server.close(); }
});
