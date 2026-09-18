import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer } from './support/isolated-vite-server.js';

test('provider sync prices the complete directory in batches and keeps directory failures separate', { timeout: 60_000 }, async () => {
  const root = fileURLToPath(new URL('..', import.meta.url));
  const server = await createIsolatedFixtureServer({ root, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'zh-CN'));
    const models = Array.from({ length: 501 }, (_, index) => ({ id: `model-${index}`, protocol: 'openai' }));
    let failCatalog = false;
    let syncs = 0;
    const priced: string[][] = [];
    await page.route('**/internal/v1/upstreams/catalog-account/models**', async route => {
      const url = new URL(route.request().url());
      const syncing = route.request().method() === 'POST';
      if (syncing) syncs++;
      if (!syncing) assert.equal(url.searchParams.get('limit'), '10000');
      await route.fulfill({ json: {
        status: failCatalog ? 'stale' : 'ready', error_code: failCatalog ? 'connection_failed' : null,
        last_success_at: 1_800_000_000_000, models: syncing ? models.slice(0, 100) : models,
      } });
    });
    await page.route('**/internal/v1/model-prices/sync', async route => {
      const body = route.request().postDataJSON();
      assert.equal(body.currency, 'USD');
      assert.equal(body.tenant_external_id, 'fixture');
      assert.ok(body.models.length > 0 && body.models.length <= 500);
      priced.push(body.models);
      await route.fulfill({ json: { imported: body.models.length, matched: body.models, preserved: [], unmatched: [], candidates: [], sources: [] } });
    });
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/provider-model-catalog.html`);
    const sync = page.getByRole('button', { name: '同步模型及价格', exact: true });
    await sync.click();
    await page.waitForFunction(() => !document.querySelector<HTMLButtonElement>('button')?.disabled);
    assert.equal(syncs, 1);
    assert.deepEqual(priced.map(batch => batch.length), [500, 1]);
    assert.equal(new Set(priced.flat()).size, 501);
    failCatalog = true;
    await sync.click();
    await page.waitForFunction(() => !document.querySelector<HTMLButtonElement>('button')?.disabled);
    assert.equal(syncs, 2);
    assert.equal(priced.length, 2, 'a failed directory refresh must not start a price sync');
  } finally {
    await browser.close();
    await server.close();
  }
});
