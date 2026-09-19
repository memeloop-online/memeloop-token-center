import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium, type Page } from 'playwright';
import { createIsolatedFixtureServer } from './support/isolated-vite-server.js';

async function openFixture() {
  const root = fileURLToPath(new URL('..', import.meta.url));
  const server = await createIsolatedFixtureServer({ root, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  const page = await browser.newPage();
  await page.addInitScript(() => { if (!localStorage.getItem('mtc-locale')) localStorage.setItem('mtc-locale', 'zh-CN'); });
  return { server, browser, page, url: `http://127.0.0.1:${address.port}/e2e/fixtures/managed-model-sync.html` };
}

function syncResponse(overrides: { warnings?: string[]; models?: number } = {}) {
  const count = overrides.models ?? 2;
  return {
    catalog: {
      account_id: 'managed-account', status: 'ready', credential_generation: 1,
      last_attempt_at: 1_800_000_000_000, last_success_at: 1_800_000_000_000, expires_at: 1_800_086_400_000,
      error_code: null,
      models: Array.from({ length: count }, (_, index) => ({ id: `catalog-model-${index}`, protocol: 'openai' })),
      disabled_models: [],
    },
    routes: { added: 1, disabled: 0, restored: 0, unchanged: count - 1, skipped: 0, warnings: overrides.warnings ?? [] },
    price_sync: { status: 'deferred', currency: 'USD', imported: 0, preserved: 0, unmatched: 0, ambiguous: 0, failed_sources: [], error_code: 'managed_route_price_sync_deferred' },
  };
}

async function stubBrowseCatalog(page: Page) {
  await page.route('**/internal/v1/upstreams/browse-account/models**', async route => {
    if (route.request().method() !== 'GET') return route.fallback();
    return route.fulfill({ json: {
      account_id: 'browse-account', status: 'ready', credential_generation: 1,
      last_attempt_at: 1_800_000_000_000, last_success_at: 1_800_000_000_000, expires_at: 1_800_086_400_000,
      error_code: null,
      models: [{ id: 'catalog-model-managed', protocol: 'openai' }, { id: 'catalog-model-fresh', protocol: 'anthropic' }],
      disabled_models: [],
    } });
  });
  await page.route('**/internal/v1/model-routes**', async route => {
    if (route.request().method() !== 'GET') return route.fallback();
    return route.fulfill({ json: [{
      id: 'route-existing', tenant_external_id: 'fixture-a', public_model: 'catalog-model-managed',
      upstream_account_ids: ['browse-account'], upstream_model: 'catalog-model-managed', protocol: 'openai',
      priority: 0, enabled: true, created_at: 1_800_000_000_000, updated_at: 1_800_000_000_000, grant_revision: 0,
    }] });
  });
}

test('managed sync shows per-account catalog and route outcomes with deferred pricing', { timeout: 60_000 }, async () => {
  const { server, browser, page, url } = await openFixture();
  try {
    await stubBrowseCatalog(page);
    const pageErrors: string[] = [];
    page.on('pageerror', error => pageErrors.push(error.message));
    let legacyPriceSyncRequests = 0;
    page.on('request', (request) => {
      if (new URL(request.url()).pathname === '/internal/v1/model-prices/sync') legacyPriceSyncRequests += 1;
    });
    let warnings: string[] = [];
    let malformed = false;
    let fail = false;
    const syncTenants: string[] = [];
    await page.route('**/internal/v1/upstreams/managed-account/models/sync-routes**', async route => {
      const request = route.request();
      assert.equal(request.method(), 'POST');
      syncTenants.push(new URL(request.url()).searchParams.get('tenant_external_id') ?? '');
      if (fail) return route.fulfill({ status: 502, json: { error: { code: 'upstream_unavailable', message: '上游暂不可用，请稍后重试。' } } });
      if (malformed) return route.fulfill({ json: { catalog: { account_id: 'managed-account' } } });
      return route.fulfill({ json: syncResponse({ warnings }) });
    });
    await page.goto(url);

    const sync = page.getByRole('button', { name: '同步模型', exact: true });
    await sync.waitFor({ state: 'visible' });
    await sync.click();
    await page.getByText('同步完成', { exact: true }).waitFor();
    await page.getByText('目录 2 个模型 · 新增 1 · 停用 0 · 恢复 0 · 保留 1 · 跳过 0', { exact: true }).waitFor();
    await page.getByText('价格同步待处理', { exact: true }).waitFor();
    assert.deepEqual(syncTenants, ['fixture-a']);
    assert.equal(legacyPriceSyncRequests, 0, 'managed sync defers pricing without legacy price requests');

    warnings = ['sync_in_progress', 'operator_route_preserved'];
    await sync.click();
    await page.getByText('同步完成，部分内容需要关注', { exact: true }).waitFor();
    await page.getByText('另一次同步正在进行，本次跳过路由核对，请稍后重试。', { exact: true }).waitFor();
    await page.getByText('部分路由由管理员手动管理，已保持原样。', { exact: true }).waitFor();

    malformed = true;
    await sync.click();
    await page.getByRole('alert').filter({ hasText: '目录响应格式有误' }).waitFor();

    malformed = false;
    fail = true;
    await sync.click();
    await page.getByRole('alert').filter({ hasText: '上游暂不可用' }).waitFor();
    assert.deepEqual(pageErrors, []);
  } finally {
    await browser.close();
    await server.close();
  }
});

test('a tenant switch discards an in-flight managed sync instead of polluting the page', { timeout: 60_000 }, async () => {
  const { server, browser, page, url } = await openFixture();
  try {
    await stubBrowseCatalog(page);
    const pageErrors: string[] = [];
    page.on('pageerror', error => pageErrors.push(error.message));
    let releaseSync = () => {};
    const syncStarted = new Promise<void>((resolve) => {
      void page.route('**/internal/v1/upstreams/managed-account/models/sync-routes**', async route => {
        resolve();
        await new Promise<void>((release) => { releaseSync = release; });
        return route.fulfill({ json: syncResponse({ models: 9 }) });
      });
    });
    await page.goto(url);
    await page.getByRole('button', { name: '同步模型', exact: true }).click();
    await page.getByText('正在同步模型与路由…', { exact: true }).waitFor();
    await syncStarted;

    await page.getByRole('button', { name: '切换租户', exact: true }).click();
    await page.getByText('正在同步模型与路由…', { exact: true }).waitFor({ state: 'detached' });
    releaseSync();
    await page.waitForTimeout(500);
    assert.equal(await page.getByText('目录 9 个模型', { exact: false }).count(), 0, 'a stale sync response must stay hidden after the tenant switch');
    assert.equal(await page.getByRole('alert').count(), 0, 'an aborted sync must not surface as a failure');
    assert.deepEqual(pageErrors, []);
  } finally {
    await browser.close();
    await server.close();
  }
});

test('catalog browsing offers view for managed routes and add for uncovered models', { timeout: 60_000 }, async () => {
  const { server, browser, page, url } = await openFixture();
  try {
    await stubBrowseCatalog(page);
    const pageErrors: string[] = [];
    page.on('pageerror', error => pageErrors.push(error.message));
    await page.goto(url);

    await page.getByRole('button', { name: '查看目录（2）', exact: true }).click();
    const managedRow = page.locator('.provider-catalog-models li', { hasText: 'catalog-model-managed' });
    const freshRow = page.locator('.provider-catalog-models li', { hasText: 'catalog-model-fresh' });
    await managedRow.getByRole('button', { name: '查看路由', exact: true }).waitFor();
    await freshRow.getByRole('button', { name: '添加路由', exact: true }).waitFor();

    await freshRow.getByRole('button', { name: '添加路由', exact: true }).click();
    await page.waitForFunction(() => document.getElementById('last-action')?.textContent?.includes('catalog-model-fresh'));
    const create = JSON.parse(await page.locator('#last-action').textContent() ?? '{}');
    assert.deepEqual(create, { kind: 'create', model: { id: 'catalog-model-fresh', protocol: 'anthropic' }, protocol: 'anthropic' });

    await managedRow.getByRole('button', { name: '查看路由', exact: true }).click();
    await page.waitForFunction(() => document.getElementById('last-action')?.textContent?.includes('route-existing'));
    const view = JSON.parse(await page.locator('#last-action').textContent() ?? '{}');
    assert.deepEqual(view, { kind: 'view', routeId: 'route-existing' });
    assert.deepEqual(pageErrors, []);
  } finally {
    await browser.close();
    await server.close();
  }
});
