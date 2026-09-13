import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

for (const model of ['gpt-5.6-luna', 'gpt-5.6-terra']) for (const confirmed of [false, true]) test(`${model} with stored confirmation=${confirmed} requires resolved evidence and retains retry focus`, { timeout: 30_000 }, async () => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required');
    return test.skip('Chromium is not installed');
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    page.setDefaultTimeout(5_000);
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    const reads: string[] = [];
    const writes: string[] = [];
    let announceRead: (release: () => void) => void;
    const nextRead = () => new Promise<() => void>((resolve) => { announceRead = resolve; });
    let pendingRead = nextRead();
    let response: 'unknown' | 'ready' | 'failed' | 'empty' = 'unknown';
    await page.route('**/internal/v1/**', async (route) => {
      const request = route.request();
      if (request.method() !== 'GET') { writes.push(request.method()); return route.abort(); }
      if (new URL(request.url()).pathname.startsWith('/internal/v1/upstreams/')) return route.fulfill({ contentType: 'application/json', body: JSON.stringify({status:'ready',models:[{id:model,protocol:'openai'}]}) });
      reads.push(request.url());
      await new Promise<void>((resolve) => { announceRead(resolve); });
      if (response === 'failed') return route.fulfill({ status: 503, contentType: 'application/json', body: JSON.stringify({ error: { message: 'Catalog temporarily unavailable' } }) });
      return route.fulfill({ contentType: 'application/json', body: JSON.stringify({
        data: response === 'empty' || response === 'unknown' ? [] : [{ id: model, protocol: 'openai', supported_account_count: 1, eligible_account_count: 1, complete_coverage: true, context_window: null, reservation_token_bound: null }],
        eligible_account_count: 1, unknown_account_count: response === 'unknown' ? 1 : 0,
        unsupported_account_count: 0,
        stale_account_count: response === 'ready' ? 1 : 0,
      }) });
    });
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/route-model-validation.html?model=${model}&confirmed=${confirmed}`);
    const save = page.getByRole('button', { name: 'Save route', exact: true });
    const assertPending = async () => {
      assert.equal(await save.isDisabled(), true);
      assert.equal(await page.locator('.custom-model-confirm').count(), 0);
      assert.equal(await page.locator('[aria-invalid="true"]').count(), 0);
      assert.equal(await page.locator('[data-custom]').textContent(), 'false');
    };
    await assertPending();
    await page.waitForFunction(() => document.querySelector('.catalog-status')?.textContent?.includes('Searching'));
    // The request is deliberately held: advancing is controlled by the test,
    // not a larger wall-clock sleep or a timing-dependent server fixture.
    (await pendingRead)();
    const retry = page.getByRole('button', { name: 'Retry', exact: true });
    await retry.waitFor();
    await assertPending();
    assert.match(await page.locator('.catalog-status').textContent() ?? '', /lack a current catalog snapshot/);
    response = 'ready';
    pendingRead = nextRead();
    await retry.focus();
    await retry.press('Enter');
    await assertPending();
    assert.equal(await retry.getAttribute('aria-disabled'), 'true');
    assert.equal(await retry.evaluate(element => element === document.activeElement), true, 'retry remains focused during debounce');
    const releaseRetry = await pendingRead;
    await retry.press('Enter');
    assert.equal(reads.length, 2, 'loading retry cannot issue another read');
    releaseRetry();
    await save.waitFor();
    await page.waitForFunction(() => !document.querySelector<HTMLButtonElement>('main > button:last-of-type')?.disabled);
    assert.equal(await retry.evaluate(element => element === document.activeElement), true, 'successful retry preserves keyboard focus');
    assert.equal(await page.locator('[data-custom]').textContent(), 'false', 'stale listed model stays catalog-restricted');
    const picker = page.getByRole('combobox');
    await picker.focus();
    await picker.press('ArrowDown');
    await page.getByRole('option').first().waitFor();
    await picker.press('Enter');
    assert.equal(await picker.inputValue(), model);
    assert.equal(await picker.getAttribute('aria-expanded'), 'false');
    await picker.press('ArrowDown');
    await picker.press('Escape');
    assert.equal(await picker.evaluate(element => element === document.activeElement), true);
    assert.equal(await picker.getAttribute('aria-expanded'), 'false');
    response = 'failed';
    pendingRead = nextRead();
    await page.getByRole('button', { name: 'Change account' }).click();
    await assertPending();
    (await pendingRead)();
    await retry.waitFor();
    await assertPending();
    response = 'empty';
    pendingRead = nextRead();
    await retry.focus();
    await retry.press('Enter');
    await assertPending();
    assert.equal(await retry.evaluate(element => element === document.activeElement), true);
    (await pendingRead)();
    const confirmation = page.locator('.custom-model-confirm input');
    await confirmation.waitFor();
    assert.equal(await save.isDisabled(), true);
    await confirmation.check();
    assert.equal(await save.isEnabled(), true);
    assert.equal(reads.length, 4);
    assert.deepEqual(writes, [], 'retry must never synchronize upstream catalogs or save a route');
  } finally {
    await browser.close();
    await server.close();
  }
});

test('authoritative unsupported generation discovery requires explicit consent and never exempts text', { timeout: 30_000 }, async () => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required');
    return test.skip('Chromium is not installed');
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    page.setDefaultTimeout(5_000);
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.route('**/internal/v1/**', route => {
      assert.equal(route.request().method(), 'GET');
      return route.fulfill({ contentType: 'application/json', body: JSON.stringify({ data: [], eligible_account_count: 1, unknown_account_count: 1, unsupported_account_count: 1, stale_account_count: 0 }) });
    });
    for (const model of ['browser-workflow-v1', 'seedance-browser-v1']) {
      await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/route-model-validation.html?model=${model}&protocol=generation&confirmed=false`);
      const confirmation = page.locator('.custom-model-confirm input');
      await confirmation.waitFor();
      const save = page.getByRole('button', { name: 'Save route', exact: true });
      assert.equal(await save.isDisabled(), true);
      await confirmation.check();
      assert.equal(await save.isEnabled(), true);
      assert.equal(await page.locator('[data-custom]').textContent(), 'true');
    }
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/route-model-validation.html?model=gpt-5.6-luna&protocol=openai&confirmed=true`);
    await page.getByRole('button', { name: 'Retry', exact: true }).waitFor();
    assert.equal(await page.getByRole('button', { name: 'Save route', exact: true }).isDisabled(), true);
    assert.equal(await page.locator('.custom-model-confirm').count(), 0);
    assert.equal(await page.locator('[data-custom]').textContent(), 'false');
  } finally {
    await browser.close();
    await server.close();
  }
});
