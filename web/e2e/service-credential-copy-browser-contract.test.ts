import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer } from './support/isolated-vite-server.js';

test('service credential copying uses original values and explains unavailable or unauthorized actions', { timeout: 60_000 }, async () => {
  const server = await createIsolatedFixtureServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const origin = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  try {
    for (const mode of ['', 'forbidden', 'unavailable', 'clipboard-failure']) {
      const page = await browser.newPage();
      await page.route('**/*', route => {
        const url = new URL(route.request().url());
        return url.origin === origin && !url.pathname.startsWith('/internal/') ? route.continue() : route.abort();
      });
      await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
      await page.goto(`${origin}/e2e/fixtures/operator-credential-workspace.html?scenario=service-copy&${mode}`);
      const copy = page.locator('.managed-resource').getByRole('button', { name: 'Copy credential', exact: true });
      await copy.waitFor();
      assert.equal(await page.getByText('Not billed', { exact: true }).count(), 1);
      assert.equal(await page.getByText('service-existing', { exact: true }).count(), 0, 'technical service identifiers stay in the tooltip');
      if (mode === 'unavailable') {
        assert.equal(await copy.isEnabled(), false);
        await copy.locator('..').focus();
        await page.getByRole('tooltip').filter({ hasText: 'The original service credential value is unavailable and cannot be copied.' }).waitFor();
      } else {
        await copy.click();
        if (mode === 'forbidden') {
          await page.getByRole('alert').getByText('Copying a service credential requires a global management credential with service-token write permission.', { exact: true }).waitFor();
          assert.equal(await page.locator('.one-time').count(), 0);
        } else if (mode === 'clipboard-failure') {
          await page.getByText('mts_service_original', { exact: true }).waitFor();
          await page.getByRole('status').filter({ hasText: 'Copy this credential manually' }).waitFor();
        } else {
          await page.getByRole('status').filter({ hasText: 'Copied Existing service credential.' }).waitFor();
          assert.equal(await page.locator('html').getAttribute('data-copied-service-fixture'), 'true');
          assert.equal(await page.locator('.one-time').count(), 0);
        }
      }
      assert.equal(await page.getByRole('dialog').count(), 0, 'copy never asks for a recovery or rotation confirmation');
      const writes = await page.evaluate(() => window.credentialFixture.requests.filter(request => request.method !== 'GET'));
      assert.equal(writes.length, mode === 'unavailable' ? 0 : 1);
      if (writes.length) {
        assert.equal(writes[0].path, '/internal/v1/service-tokens/service-existing/copy');
        assert.equal(writes[0].cache, 'no-store');
        assert.equal(writes[0].hasSignal, true);
      }
      await page.close();
    }
  } finally { await browser.close(); await server.close(); }
});
