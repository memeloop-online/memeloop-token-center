import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium, type Page } from 'playwright';
import { createIsolatedFixtureServer } from './support/isolated-vite-server.js';
import type { UpstreamCatalogPriceSync } from '../src/types.js';

async function waitForEnabledButton(page: Page, name: string) {
  await page.getByRole('button', { name, exact: true }).waitFor({ state: 'visible' });
  await page.waitForFunction((label) => {
    const button = [...document.querySelectorAll<HTMLButtonElement>('button')]
      .find((candidate) => candidate.textContent?.trim() === label);
    return Boolean(button && !button.disabled);
  }, name);
}

test('provider sync consumes its combined catalog result and keeps price outcomes separate', { timeout: 60_000 }, async () => {
  const root = fileURLToPath(new URL('..', import.meta.url));
  const server = await createIsolatedFixtureServer({ root, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    await page.addInitScript(() => { if (!localStorage.getItem('mtc-locale')) localStorage.setItem('mtc-locale', 'zh-CN'); });
    const models = Array.from({ length: 501 }, (_, index) => ({ id: `model-${index}`, protocol: 'openai', context_window: null, reservation_token_bound: null, reservation_bound_source: null }));
    const disabledModels = [{ id: 'retired-model', protocol: 'openai', status: 'disabled' as const, disabled_at: 1_800_000_000_000, reason: 'removed_from_upstream' as const }];
    const pageErrors: string[] = [];
    page.on('pageerror', error => pageErrors.push(error.message));
    let malformedRead = true;
    let failCatalog = false;
    let priceStatus: UpstreamCatalogPriceSync['status'] = 'partial';
    let syncs = 0;
    let reads = 0;
    let legacyPriceSyncRequests = 0;
    page.on('request', (request) => {
      if (new URL(request.url()).pathname === '/internal/v1/model-prices/sync') legacyPriceSyncRequests += 1;
    });
    await page.route('**/internal/v1/upstreams/catalog-account/models**', async route => {
      const url = new URL(route.request().url());
      const syncing = route.request().method() === 'POST';
      if (syncing) syncs++;
      else { reads++; assert.equal(url.searchParams.get('limit'), '10000'); }
      if (!syncing && malformedRead) return route.fulfill({ json: [] });
      const catalog = {
        status: failCatalog ? 'stale' : 'ready', error_code: failCatalog ? 'connection_failed' : null,
        account_id: 'catalog-account', credential_generation: 1, last_attempt_at: 1_800_000_000_000,
        last_success_at: 1_800_000_000_000, expires_at: 1_800_086_400_000,
        models: syncing ? models : models.slice(0, 100), disabled_models: disabledModels,
      };
      if (!syncing) return route.fulfill({ json: catalog });
      const price_sync: UpstreamCatalogPriceSync = failCatalog
        ? { status: 'skipped', currency: 'USD', imported: 0, preserved: 0, unmatched: 0, ambiguous: 0, failed_sources: [], error_code: null }
        : priceStatus === 'error'
          ? { status: 'error', currency: 'USD', imported: 0, preserved: 0, unmatched: 0, ambiguous: 0, failed_sources: [], error_code: 'price_sync_failed' }
          : { status: priceStatus, currency: 'USD', imported: 499, preserved: 1, unmatched: 1, ambiguous: 1, failed_sources: ['litellm'], error_code: null };
      await route.fulfill({ json: { ...catalog, price_sync } });
    });
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/provider-model-catalog.html`);
    const sync = page.getByRole('button', { name: '同步模型及价格', exact: true });
    await page.getByRole('alert').filter({ hasText: '目录响应格式有误' }).waitFor();
    assert.deepEqual(pageErrors, [], 'malformed optional metadata must not crash the account workspace');
    malformedRead = false;

    await waitForEnabledButton(page, '同步模型及价格');
    await sync.click();
    await page.getByText('已同步 501 个模型', { exact: true }).waitFor();
    await page.getByRole('status').filter({ hasText: '价格部分就绪' }).waitFor();
    await page.getByRole('status').filter({ hasText: '价格源暂不可用：litellm' }).waitFor();
    await waitForEnabledButton(page, '同步模型及价格');
    assert.equal(syncs, 1);
    assert.equal(reads, 1, 'the sync response is the current catalog; do not read it again');
    assert.equal(legacyPriceSyncRequests, 0, 'the server prices the full directory as part of catalog sync');

    await page.getByRole('button', { name: '已停用模型（1）', exact: true }).click();
    await page.getByText('retired-model', { exact: true }).waitFor();

    failCatalog = true;
    await sync.click();
    await page.getByRole('alert').filter({ hasText: '模型: 连接失败' }).waitFor();
    await page.getByRole('status').filter({ hasText: '未执行价格同步' }).waitFor();
    await waitForEnabledButton(page, '同步模型及价格');
    assert.equal(syncs, 2);
    assert.equal(legacyPriceSyncRequests, 0, 'a failed directory refresh cannot fall back to price-sync requests');

    failCatalog = false;
    priceStatus = 'error';
    await sync.click();
    await page.getByText('已同步 501 个模型', { exact: true }).waitFor();
    await page.getByRole('status').filter({ hasText: '价格同步失败' }).waitFor();
    await page.getByRole('alert').filter({ hasText: '价格同步未完成' }).waitFor();
    await waitForEnabledButton(page, '同步模型及价格');
    await page.getByRole('alert').filter({ hasText: '模型:' }).waitFor({ state: 'detached' });
    assert.equal(legacyPriceSyncRequests, 0);

    priceStatus = 'partial';
    await page.evaluate(() => localStorage.setItem('mtc-locale', 'en'));
    await page.reload();
    await waitForEnabledButton(page, 'Sync models and prices');
    await page.getByRole('button', { name: 'Sync models and prices', exact: true }).click();
    await page.getByRole('status').filter({ hasText: 'Prices partially ready' }).waitFor();
    await page.getByRole('status').filter({ hasText: 'Price sources unavailable: litellm' }).waitFor();
    await waitForEnabledButton(page, 'Sync models and prices');
    assert.deepEqual(pageErrors, []);
    assert.equal(legacyPriceSyncRequests, 0);
  } finally {
    await browser.close();
    await server.close();
  }
});

test('removed models do not enter the actual route picker aggregate', { timeout: 60_000 }, async () => {
  const root = fileURLToPath(new URL('..', import.meta.url));
  const server = await createIsolatedFixtureServer({ root, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    let retiredSearches = 0;
    let releaseRetiredSearch = () => {};
    const retiredSearchStarted = new Promise<void>((resolve) => { releaseRetiredSearch = resolve; });
    let completeRetiredSearch = () => {};
    const retiredSearchCanComplete = new Promise<void>((resolve) => { completeRetiredSearch = resolve; });
    await page.route('**/internal/v1/**', async route => {
      const request = route.request();
      assert.equal(request.method(), 'GET');
      const url = new URL(request.url());
      if (url.pathname === '/internal/v1/upstream-models') {
        const query = url.searchParams.get('q') ?? '';
        if (query === 'retired-model') {
          retiredSearches += 1;
          releaseRetiredSearch();
          await retiredSearchCanComplete;
          return route.fulfill({ json: { data: [], eligible_account_count: 1, unknown_account_count: 0, unsupported_account_count: 0, stale_account_count: 0 } });
        }
        return route.fulfill({ json: {
          data: [{ id: 'active-model', protocol: 'openai', supported_account_count: 1, eligible_account_count: 1, complete_coverage: true, context_window: null, reservation_token_bound: null }],
          eligible_account_count: 1, unknown_account_count: 0, unsupported_account_count: 0, stale_account_count: 0,
        } });
      }
      if (url.pathname === '/internal/v1/upstreams/first/models') {
        return route.fulfill({ json: { status: 'ready', models: [{ id: 'active-model', protocol: 'openai' }], disabled_models: [{ id: 'retired-model', protocol: 'openai', status: 'disabled', disabled_at: 1_800_000_000_000, reason: 'removed_from_upstream' }] } });
      }
      return route.fulfill({ json: {} });
    });
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/route-model-validation.html?model=active-model&confirmed=false`);
    const picker = page.getByRole('combobox', { name: 'Upstream model', exact: true });
    await picker.click();
    await page.getByRole('option', { name: /active-model/i }).waitFor();
    await picker.fill('retired-model');
    await retiredSearchStarted;
    assert.equal(retiredSearches, 1, 'the editable route picker searches the aggregate endpoint, not the disabled catalog history');
    completeRetiredSearch();
    await page.getByText('No matching catalog model', { exact: true }).waitFor();
    assert.equal(await page.getByRole('option').count(), 0, 'the aggregate-backed route picker does not offer a removed model');
    assert.equal(await page.getByRole('button', { name: 'Save route', exact: true }).isDisabled(), true, 'a removed model cannot become a route through picker selection');
  } finally {
    await browser.close();
    await server.close();
  }
});
