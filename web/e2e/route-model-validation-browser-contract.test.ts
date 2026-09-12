import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

for (const model of ['gpt-5.6-luna', 'gpt-5.6-terra']) test(`${model} waits for scoped evidence and recovers with a read-only retry`, { timeout: 30_000 }, async () => {
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
    let response: 'ready' | 'failed' | 'empty' = 'ready';
    await page.route('**/internal/v1/**', async (route) => {
      const request = route.request();
      if (request.method() !== 'GET') { writes.push(request.method()); return route.abort(); }
      if (new URL(request.url()).pathname.startsWith('/internal/v1/upstreams/')) return route.fulfill({ contentType: 'application/json', body: JSON.stringify({status:'ready',models:[{id:model,protocol:'openai'}]}) });
      reads.push(request.url());
      await new Promise<void>((resolve) => { announceRead(resolve); });
      if (response === 'failed') return route.fulfill({ status: 503, contentType: 'application/json', body: JSON.stringify({ error: { message: 'Catalog temporarily unavailable' } }) });
      return route.fulfill({ contentType: 'application/json', body: JSON.stringify({ data: response === 'empty' ? [] : [{ id: model, protocol: 'openai', complete_coverage: true }], stale_account_count: 0 }) });
    });
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/route-model-validation.html?model=${model}`);
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
    await save.waitFor();
    await page.waitForFunction(() => !document.querySelector<HTMLButtonElement>('main > button:last-of-type')?.disabled);
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
    const retry = page.getByRole('button', { name: 'Retry', exact: true });
    await retry.waitFor();
    await assertPending();
    response = 'empty';
    pendingRead = nextRead();
    await retry.click();
    await assertPending();
    (await pendingRead)();
    const confirmation = page.locator('.custom-model-confirm input');
    await confirmation.waitFor();
    assert.equal(await save.isDisabled(), true);
    await confirmation.check();
    assert.equal(await save.isEnabled(), true);
    assert.equal(reads.length, 3);
    assert.deepEqual(writes, [], 'retry must never synchronize upstream catalogs or save a route');
  } finally {
    await browser.close();
    await server.close();
  }
});
