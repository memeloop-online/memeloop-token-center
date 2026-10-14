import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer } from './support/isolated-vite-server.js';
import { requestEgressCopy } from '../src/operator/requestEgressCopy.js';
import './support/request-egress-fixtures.js';

test('operator exit history keeps addresses, coverage, scope and failed reads distinct with bilingual narrow screenshots', { timeout: 90_000 }, async () => {
  const server = await createIsolatedFixtureServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const origin = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  const artifactRoot = fileURLToPath(new URL('../e2e-artifacts/request-diagnostics/', import.meta.url));
  await mkdir(artifactRoot, { recursive: true });
  try {
    for (const locale of ['zh-CN', 'en']) {
      const page = await browser.newPage({ viewport: { width: 390, height: 844 } });
      const copy = requestEgressCopy(locale);
      const errors: string[] = [];
      page.on('pageerror', error => errors.push(error.message));
      await page.route('**/*', route => new URL(route.request().url()).origin === origin ? route.continue() : route.abort());
      await page.addInitScript(value => localStorage.setItem('mtc-locale', value), locale);
      await page.goto(`${origin}/e2e/fixtures/request-egress.html`);
      await page.getByText('Current exit · socks5h://current.proxy.test:1080', { exact: true }).waitFor();
      const summary = page.getByRole('region', { name: copy.title, exact: true });
      await summary.locator('[data-egress-summary]').first().waitFor();
      assert.equal(await summary.locator('[data-egress-summary]').count(), 3);
      assert.match(await summary.innerText(), /old\.proxy\.test|later\.proxy\.test/);
      assert.doesNotMatch(await summary.innerText(), /current\.proxy\.test|credential-canary/);
      assert.ok((await summary.innerText()).includes(copy.coverage(2, 3)));
      assert.ok((await summary.innerText()).includes('0 B'));
      await summary.getByLabel(copy.groupFilter, { exact: true }).selectOption('group-a');
      assert.equal(await summary.locator('[data-egress-summary]').count(), 2);
      await page.screenshot({ path: `${artifactRoot}/egress-summary-${locale}-390.png`, fullPage: true });
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1), true);
      for (const mode of ['denied', 'failed', 'empty']) {
        await page.evaluate(value => { window.egressFixture.mode = value; }, mode);
        await summary.getByRole('button', { name: copy.refresh, exact: true }).click();
        await summary.getByText(mode === 'empty' ? copy.empty : mode === 'denied' ? copy.denied : copy.failed, { exact: true }).waitFor();
        assert.equal(await summary.locator('[data-egress-summary]').count(), 0);
        assert.doesNotMatch(await summary.innerText(), /credential-canary/);
        await page.screenshot({ path: `${artifactRoot}/egress-${mode}-${locale}-390.png`, fullPage: true });
      }
      await page.evaluate(() => { window.egressFixture.mode = 'slow'; });
      await summary.getByRole('button', { name: copy.refresh, exact: true }).click();
      await page.waitForFunction(() => Boolean(window.egressFixture.release));
      await summary.getByRole('group', { name: copy.loading, exact: true }).waitFor();
      await page.screenshot({ path: `${artifactRoot}/egress-loading-${locale}-390.png`, fullPage: true });
      await page.evaluate(() => { window.egressFixture.mode = 'ready'; window.egressFixture.switchTenant(); });
      await summary.getByText(copy.empty, { exact: true }).waitFor();
      await page.evaluate(() => window.egressFixture.release?.());
      assert.equal(await summary.locator('[data-egress-summary]').count(), 0);
      assert.ok(await page.evaluate(() => window.egressFixture.reads.every(query => new URLSearchParams(query).has('tenant_external_id'))));
      await page.goto(`${origin}/e2e/fixtures/request-lifecycle.html?egress=1`);
      const row = page.locator('tbody tr').filter({ has: page.getByText('model-a', { exact: true }) });
      await row.locator('.table-action').click();
      const drawer = page.getByRole('dialog', { name: 'model-a' });
      await drawer.locator('[data-egress-attempt]').first().waitFor();
      assert.equal(await drawer.locator('[data-egress-attempt]').count(), 2);
      assert.ok((await drawer.innerText()).includes(copy.final));
      assert.ok((await drawer.innerText()).includes(copy.noEof));
      assert.match(await drawer.innerText(), /HTTP\/2/);
      assert.doesNotMatch(await drawer.innerText(), /credential-canary|attempt-a|attempt-b/);
      await drawer.getByRole('region', { name: copy.attemptsTitle, exact: true }).scrollIntoViewIfNeeded();
      await page.mouse.move(0, 0);
      await page.screenshot({ path: `${artifactRoot}/egress-attempts-${locale}-390.png`, fullPage: true });
      assert.deepEqual(errors, []);
      await page.close();
    }
  } finally { await browser.close(); await server.close(); }
});
