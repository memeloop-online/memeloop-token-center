import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer } from './support/isolated-vite-server.js';
import type { ModelPriceSyncResult } from '../src/types.js';

test('provider sync prices the complete directory in batches and keeps directory failures separate', { timeout: 60_000 }, async () => {
  const root = fileURLToPath(new URL('..', import.meta.url));
  const server = await createIsolatedFixtureServer({ root, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    await page.addInitScript(() => { if (!localStorage.getItem('mtc-locale')) localStorage.setItem('mtc-locale', 'zh-CN'); });
    const models = Array.from({ length: 501 }, (_, index) => ({ id: `model-${index}`, protocol: 'openai' }));
    const pageErrors: string[] = [];
    page.on('pageerror', error => pageErrors.push(error.message));
    let malformedRead = true;
    let emptyCatalog = false;
    let failCatalog = false;
    let syncs = 0;
    const priced: string[][] = [];
    await page.route('**/internal/v1/upstreams/catalog-account/models**', async route => {
      const url = new URL(route.request().url());
      const syncing = route.request().method() === 'POST';
      if (syncing) syncs++;
      if (!syncing) assert.equal(url.searchParams.get('limit'), '10000');
      if (!syncing && malformedRead) return route.fulfill({ json: [] });
      await route.fulfill({ json: {
        status: failCatalog ? 'stale' : 'ready', error_code: failCatalog ? 'connection_failed' : null,
        last_success_at: 1_800_000_000_000, models: emptyCatalog ? [] : syncing ? models.slice(0, 100) : models,
      } });
    });
    await page.route('**/internal/v1/model-prices/sync', async route => {
      const body = route.request().postDataJSON();
      assert.equal(body.currency, 'USD');
      assert.equal(body.tenant_external_id, 'fixture');
      assert.ok(body.models.length > 0 && body.models.length <= 500);
      priced.push(body.models);
      const result: ModelPriceSyncResult = {
        source: 'models.dev', sources: ['models.dev'], imported: body.models.length,
        matched: body.models, preserved: [], unmatched: [], candidates: [], prices: [],
        sourceResults: [
          { source: 'models.dev', models: body.models.length, skipped: 0 },
          { source: 'litellm', models: 0, skipped: 0, error: 'source unavailable' },
          { source: 'openrouter', models: 0, skipped: 0, error: 'source unavailable' },
        ],
      };
      await route.fulfill({ json: result });
    });
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/provider-model-catalog.html`);
    const sync = page.getByRole('button', { name: '同步模型及价格', exact: true });
    await page.getByRole('alert').filter({ hasText: '目录响应格式有误' }).waitFor();
    assert.deepEqual(pageErrors, [], 'malformed optional metadata must not crash the account workspace');
    malformedRead = false;
    await sync.click();
    await page.waitForFunction(() => !document.querySelector<HTMLButtonElement>('button')?.disabled);
    assert.equal(syncs, 1);
    assert.deepEqual(priced.map(batch => batch.length), [500, 1]);
    assert.equal(new Set(priced.flat()).size, 501);
    await page.getByRole('alert').filter({ hasText: '价格源暂不可用' }).waitFor();
    failCatalog = true;
    await sync.click();
    await page.waitForFunction(() => !document.querySelector<HTMLButtonElement>('button')?.disabled);
    assert.equal(syncs, 2);
    assert.equal(priced.length, 2, 'a failed directory refresh must not start a price sync');
    failCatalog = false; emptyCatalog = true;
    await sync.click();
    await page.getByText('价格（美元）: 目录为空', { exact: true }).waitFor();
    assert.equal(priced.length, 2, 'an empty catalog must not invoke the global all-models pricing fallback');
    emptyCatalog = false;
    await page.evaluate(() => localStorage.setItem('mtc-locale', 'en'));
    await page.reload();
    await page.getByRole('button', { name: 'Sync models and prices', exact: true }).click();
    await page.getByRole('alert').filter({ hasText: 'Price sources unavailable: litellm and openrouter' }).waitFor();
    assert.deepEqual(pageErrors, []);
  } finally {
    await browser.close();
    await server.close();
  }
});
