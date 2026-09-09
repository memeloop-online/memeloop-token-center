import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

test('plugin configuration is lazy and isolated while resource reads cancel on tenant changes', { timeout: 60_000 }, async () => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required for resource loading acceptance');
    return test.skip('Chromium is required');
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    const reads: string[] = [];
    const errors: string[] = [];
    page.on('pageerror', (error) => errors.push(error.message));
    let releaseAlpha!: () => void;
    const alphaPending = new Promise<void>((resolve) => { releaseAlpha = resolve; });
    let alphaStarted!: () => void;
    const alphaRequested = new Promise<void>((resolve) => { alphaStarted = resolve; });
    await page.route('**/fixture/resource?*', async (route) => {
      const tenant = new URL(route.request().url()).searchParams.get('tenant');
      if (tenant === 'alpha') { alphaStarted(); await alphaPending; }
      await route.fulfill({ json: { tenant } }).catch(() => { /* The alpha read was explicitly aborted by the tenant change. */ });
    });
    await page.route('**/internal/v1/plugins/*/configuration?*', async (route) => {
      const url = new URL(route.request().url());
      reads.push(`${url.pathname}:${url.searchParams.get('tenant_external_id')}`);
      await route.fulfill({ json: { source: 'tenant', scope_version: 1, value: { mode: `${url.searchParams.get('tenant_external_id')}-configuration` } } });
    });
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/resource-loading.html`);
    await alphaRequested;
    await page.getByText('first', { exact: true }).waitFor();
    assert.equal(reads.length, 0, 'catalog rendering must not fan out configuration requests');
    const first = page.locator('.managed-resource').filter({ has: page.getByText('first', { exact: true }) });
    await first.locator('summary').click();
    await first.getByLabel('Mode').waitFor();
    assert.equal(await first.getByLabel('Mode').inputValue(), 'alpha-configuration');
    assert.equal(reads.length, 1);
    await first.locator('summary').click();
    await first.locator('summary').click();
    assert.equal(reads.length, 1, 'reopening a loaded editor must reuse its scoped configuration');
    const alphaAborted = page.waitForEvent('requestfailed', (request) => request.url().includes('/fixture/resource?tenant=alpha'));
    await page.getByRole('button', { name: 'Switch tenant' }).click();
    await alphaAborted;
    await page.locator('output').filter({ hasText: 'beta' }).waitFor();
    assert.equal(await page.getByLabel('Mode').count(), 0, 'the previous tenant configuration must unmount immediately');
    await first.locator('summary').click();
    await first.getByLabel('Mode').waitFor();
    assert.equal(await first.getByLabel('Mode').inputValue(), 'beta-configuration');
    releaseAlpha();
    assert.deepEqual(reads, ['/internal/v1/plugins/first/configuration:alpha', '/internal/v1/plugins/first/configuration:beta']);
    assert.deepEqual(errors, []);
  } finally {
    await browser.close();
    await server.close();
  }
});
