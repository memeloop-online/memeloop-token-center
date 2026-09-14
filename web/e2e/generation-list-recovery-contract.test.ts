import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

test('generation refresh distinguishes failure, loading and recovered empty data', { timeout: 30_000 }, async context => {
  const executablePath = process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH ?? chromium.executablePath();
  if (!existsSync(executablePath)) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required');
    return context.skip('Chromium runtime is required');
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer!.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ executablePath, headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 390, height: 900 } });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    let fail = true;
    let release: (() => void) | undefined;
    const received = new Promise<void>(resolve => { release = resolve; });
    let finish: (() => void) | undefined;
    const pending = new Promise<void>(resolve => { finish = resolve; });
    await page.route('**/self/v1/generations?*', async route => {
      assert.equal(route.request().method(), 'GET', 'this test never writes a generation');
      if (fail) return route.fulfill({ status: 503, json: { error: { message: 'fixture unavailable' } } });
      release!();
      await pending;
      await route.fulfill({ json: [] });
    });
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/generation-list.html`);
    await page.getByRole('alert').waitFor();
    assert.equal(await page.getByText('No generation jobs', { exact: true }).count(), 0, 'failed initial load must not claim an empty result');
    fail = false;
    await page.getByRole('button', { name: 'Refresh generation jobs' }).click();
    await received;
    assert.ok(await page.getByRole('button', { name: 'Loading…' }).isDisabled());
    finish!();
    await page.getByText('No generation jobs', { exact: true }).waitFor();
    assert.equal(await page.getByRole('alert').count(), 0, 'successful refresh must clear its prior load error');
    assert.equal(await page.locator('.self-generations').evaluate(el => getComputedStyle(el).boxShadow), 'none');
  } finally { await browser.close(); await server.close(); }
});
