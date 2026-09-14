import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer as createServer } from './support/isolated-vite-server.js';

test('group strategy schema validation, CAS refresh preservation, native reset and credential isolation', { timeout: 45_000 }, async context => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    context.skip('Chromium not installed'); return;
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    page.on('pageerror', error => context.diagnostic(error.message));
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    const writes: Record<string, any>[] = [];
    let catalogReads = 0;
    await page.route('**/internal/v1/**', async route => {
      const url = route.request().url();
      if (url.endsWith('/plugins/group-routing-strategies')) {
        catalogReads++;
        await route.fulfill({ json: [{ id: 'weighted', version: 'group-routing-v1', default: { factor: 3 }, schema: { type: 'object', required: ['factor'], properties: { factor: { type: 'integer', title: 'Weight factor', minimum: 1 } } } }] });
      } else if (route.request().method() === 'PUT') {
        writes.push(route.request().postDataJSON());
        await route.fulfill(writes.length === 1 ? { status: 409, json: { error: { message: 'conflict' } } } : { json: { id: 'group', updated_at: 4, strategy_version: 5 } });
      } else await route.fulfill({ json: [{ id: 'group', updated_at: 3, strategy_version: 4 }] });
    });
    const base = `http://127.0.0.1:${address.port}/e2e/fixtures/group-strategy.html`;
    await page.goto(base);
    const picker = page.getByRole('combobox', { name: 'Group routing strategy' });
    await picker.selectOption('weighted');
    const factor = page.getByLabel('Weight factor', { exact: false });
    assert.equal(await factor.inputValue(), '3');
    assert.equal(await page.locator('.group-strategy-editor textarea').count(), 0);
    await factor.fill('0');
    await page.getByRole('button', { name: 'Save group strategy' }).click();
    assert.equal(writes.length, 0, 'schema invalid configuration cannot be sent');
    await factor.fill('7');
    await page.getByLabel('Overlapping group priority').fill('10');
    await page.getByRole('button', { name: 'Save group strategy' }).click();
    await page.getByText(/Its version was refreshed/).waitFor();
    assert.equal(await factor.inputValue(), '7');
    assert.equal(await page.getByLabel('Overlapping group priority').inputValue(), '10');
    assert.equal(writes[0].expected_strategy_version, 2);
    await page.getByRole('button', { name: 'Save group strategy' }).click();
    await page.getByText('Group strategy saved', { exact: true }).waitFor();
    assert.equal(writes[1].expected_strategy_version, 4);
    assert.equal(writes[1].expected_updated_at, 3);
    assert.deepEqual(writes[1].routing_strategy, { plugin_id: 'weighted', config: { factor: 7 } });
    await picker.selectOption('');
    await page.getByRole('button', { name: 'Save group strategy' }).click();
    await page.waitForFunction(() => document.querySelector('.group-strategy-editor [role="status"]')?.textContent === 'Group strategy saved');
    assert.equal(writes[2].routing_strategy, null);
    const readsBefore = catalogReads;
    await page.goto(`${base}?kind=credential`);
    await page.getByRole('heading', { name: 'Credential groups' }).waitFor();
    assert.equal(await page.locator('.group-strategy-editor').count(), 0);
    assert.equal(catalogReads, readsBefore, 'credential groups do not request strategy catalog');
  } finally { await browser.close(); await server.close(); }
});
