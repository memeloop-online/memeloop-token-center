import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

for (const model of ['gpt-5.6-luna', 'gpt-5.6-terra']) for (const confirmed of [false, true]) test(`${model} preserves stored policy=${confirmed} during directory failure and retains retry focus`, { timeout: 30_000 }, async () => {
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
    const assertPending = async (existing = true) => {
      assert.equal(await save.isDisabled(), !existing);
      assert.equal(await page.locator('.custom-model-confirm').count(), 0);
      assert.equal(await page.locator('[aria-invalid="true"]').count(), 0);
      assert.equal(await page.locator('[data-custom]').textContent(), String(existing && confirmed));
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
    await page.waitForFunction(() => document.querySelector('.catalog-status')?.textContent?.includes('last trusted'));
    assert.equal(await retry.evaluate(element => element === document.activeElement), true, 'successful retry preserves keyboard focus');
    assert.equal(await page.locator('[data-custom]').textContent(), String(confirmed), 'reading a catalog does not rewrite a stored routing policy');
    const picker = page.getByRole('combobox');
    await picker.focus();
    await picker.press('ArrowDown');
    await page.getByRole('option').first().waitFor();
    await picker.press('Enter');
    assert.equal(await page.locator('[data-custom]').textContent(), 'false', 'an explicit catalog choice retains catalog restrictions');
    assert.equal(await picker.inputValue(), model);
    assert.equal(await picker.getAttribute('aria-expanded'), 'false');
    await picker.press('ArrowDown');
    await picker.press('Escape');
    assert.equal(await picker.evaluate(element => element === document.activeElement), true);
    assert.equal(await picker.getAttribute('aria-expanded'), 'false');
    response = 'failed';
    pendingRead = nextRead();
    await page.getByRole('button', { name: 'Change account' }).click();
    await assertPending(false);
    (await pendingRead)();
    await retry.waitFor();
    await assertPending(false);
    response = 'empty';
    pendingRead = nextRead();
    await retry.focus();
    await retry.press('Enter');
    await assertPending(false);
    assert.equal(await retry.evaluate(element => element === document.activeElement), true);
    (await pendingRead)();
    await page.waitForFunction(() => document.querySelector('.catalog-status')?.textContent?.includes('Select or enter'));
    assert.equal(await save.isDisabled(), true);
    pendingRead = nextRead();
    await picker.fill(`${model}-manual`);
    assert.equal(await save.isEnabled(), true);
    assert.equal(await page.locator('[data-custom]').textContent(), 'true');
    assert.equal(await page.getByRole('checkbox').count(), 0);
    (await pendingRead)();
    assert.equal(reads.length, 5);
    assert.deepEqual(writes, [], 'retry must never synchronize upstream catalogs or save a route');
  } finally {
    await browser.close();
    await server.close();
  }
});

test('manual text, audio and generation models work with incomplete group or account catalogs without a checkbox', { timeout: 30_000 }, async () => {
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
    for (const protocol of ['generation', 'openai-audio', 'openai']) for (const grouped of [false, true]) {
      await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/route-model-validation.html?model=&protocol=${protocol}&confirmed=false&existing=false${grouped ? '&group=1' : ''}`);
      const save = page.getByRole('button', { name: 'Save route', exact: true });
      assert.equal(await save.isDisabled(), true);
      await page.getByRole('combobox').fill(`${protocol}-custom-model`);
      assert.equal(await save.isEnabled(), true);
      assert.equal(await page.locator('[data-custom]').textContent(), 'true');
      assert.equal(await page.getByRole('checkbox').count(), 0);
    }
  } finally {
    await browser.close();
    await server.close();
  }
});

test('account, group, exclusion, protocol and authorization scope changes cannot revive an old choice', { timeout: 30_000 }, async () => {
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.route('**/internal/v1/**', route => route.fulfill({ status: 503, contentType: 'application/json', body: JSON.stringify({ error: { message: 'Catalog temporarily unavailable' } }) }));
    for (const scope of ['account', 'group', 'exclusions', 'protocol', 'token', 'tenant']) {
      await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/route-model-validation.html?confirmed=true${scope === 'group' ? '&group=1' : ''}`);
      const save = page.getByRole('button', { name: 'Save route', exact: true });
      assert.equal(await save.isEnabled(), true);
      const change = page.getByRole('button', { name: `Change ${scope}`, exact: true });
      await change.click();
      assert.equal(await save.isDisabled(), true);
      assert.equal(await page.locator('[data-custom]').textContent(), 'false');
      await change.click();
      assert.equal(await save.isDisabled(), true, `${scope} cannot restore old consent`);
      await page.getByRole('combobox').fill('operator-selected-model');
      assert.equal(await save.isEnabled(), true);
      assert.equal(await page.locator('[data-custom]').textContent(), 'true');
    }
  } finally { await browser.close(); await server.close(); }
});
