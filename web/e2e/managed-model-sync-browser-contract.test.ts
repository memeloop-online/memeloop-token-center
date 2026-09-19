import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium, type Page } from 'playwright';
import { createIsolatedFixtureServer } from './support/isolated-vite-server.js';
import type { ManagedModelSyncResponse } from '../src/types.js';

declare global {
  interface Window { managedHandoffPayloads: Array<Record<string, unknown>>; managedHandoffRouteReads: string[] }
}

const pagedFocusRouteId = '00000000-0000-0000-0000-000000000101';

function pagedRoute(id: string, createdAt: number, publicModel = `model-${id.slice(-3)}`) {
  return {
    id, tenant_external_id: 'fixture-a', public_model: publicModel,
    upstream_account_ids: ['browse-account'], upstream_model: publicModel, protocol: 'openai',
    priority: 0, enabled: true, created_at: createdAt, updated_at: createdAt, grant_revision: 0,
  };
}

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

function syncResponse(overrides: { warnings?: string[]; models?: number } = {}): ManagedModelSyncResponse {
  const count = overrides.models ?? 2;
  return {
    catalog: {
      account_id: 'managed-account', status: 'ready', credential_generation: 1,
      last_attempt_at: 1_800_000_000_000, last_success_at: 1_800_000_000_000, expires_at: 1_800_086_400_000,
      error_code: null,
      models: Array.from({ length: count }, (_, index) => ({ id: `catalog-model-${index}`, protocol: 'openai', context_window: null, reservation_token_bound: null, reservation_bound_source: null })),
      disabled_models: [],
    },
    routes: { added: 1, disabled: 0, restored: 0, unchanged: count - 1, skipped: 0, warnings: overrides.warnings ?? [] },
    price_sync: { status: 'partial', currency: 'USD', imported: 7, preserved: 2, unmatched: 1, ambiguous: 3, failed_sources: ['litellm'], error_code: null },
  };
}

async function stubBrowseCatalog(page: Page) {
  await page.route('**/internal/v1/upstreams/browse-account/models**', async route => {
    if (route.request().method() !== 'GET') return route.fallback();
    return route.fulfill({ json: {
      account_id: 'browse-account', status: 'ready', credential_generation: 1,
      last_attempt_at: 1_800_000_000_000, last_success_at: 1_800_000_000_000, expires_at: 1_800_086_400_000,
      error_code: null,
      models: [
        { id: 'catalog-model-managed', protocol: 'openai', context_window: null, reservation_token_bound: null, reservation_bound_source: null },
        { id: 'catalog-model-fresh', protocol: 'anthropic', context_window: null, reservation_token_bound: null, reservation_bound_source: null },
        { id: 'catalog-model-unknown', protocol: 'vendor-specific', context_window: null, reservation_token_bound: null, reservation_bound_source: null },
      ],
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

test('managed sync shows per-account catalog, route, and price outcomes', { timeout: 60_000 }, async () => {
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
    let legacyPricing = false;
    let malformed = false;
    let fail = false;
    const syncTenants: string[] = [];
    await page.route('**/internal/v1/upstreams/managed-account/models/sync-routes**', async route => {
      const request = route.request();
      assert.equal(request.method(), 'POST');
      syncTenants.push(new URL(request.url()).searchParams.get('tenant_external_id') ?? '');
      if (fail) return route.fulfill({ status: 502, json: { error: { code: 'upstream_unavailable', message: '上游暂不可用，请稍后重试。' } } });
      if (malformed) return route.fulfill({ json: { catalog: { account_id: 'managed-account' } } });
      const response = syncResponse({ warnings });
      if (legacyPricing) response.price_sync = { status: 'deferred', currency: 'USD', imported: 0, preserved: 0, unmatched: 0, ambiguous: 0, failed_sources: [], error_code: 'managed_route_price_sync_deferred' };
      return route.fulfill({ json: response });
    });
    await page.goto(url);

    const sync = page.getByRole('button', { name: '同步模型', exact: true });
    await sync.waitFor({ state: 'visible' });
    await sync.click();
    await page.getByText('同步完成，部分内容需要关注', { exact: true }).waitFor();
    await page.getByText('目录 2 个模型 · 新增 1 · 停用 0 · 恢复 0 · 保留 1 · 跳过 0', { exact: true }).waitFor();
    await page.getByText('部分价格待处理 · 更新 7 · 保留 2 · 未匹配 1 · 待确认 3', { exact: true }).waitFor();
    await page.getByText('待恢复的价格源：litellm', { exact: true }).waitFor();
    assert.deepEqual(syncTenants, ['fixture-a']);
    assert.equal(legacyPriceSyncRequests, 0, 'managed sync uses the combined server-side price result');

    warnings = ['sync_in_progress', 'operator_route_preserved', 'complete_catalog_unsupported'];
    await sync.click();
    await page.getByText('同步完成，部分内容需要关注', { exact: true }).waitFor();
    await page.getByText('另一次同步正在进行，本次跳过路由核对，请稍后重试。', { exact: true }).waitFor();
    await page.getByText('部分路由由管理员手动管理，已保持原样。', { exact: true }).waitFor();
    await page.getByText('此接入方式无法确认目录完整性，本次跳过路由核对。', { exact: true }).waitFor();

    warnings = [];
    legacyPricing = true;
    await sync.click();
    await page.getByText('价格同步服务正在更新', { exact: true }).waitFor();

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

    await page.getByRole('button', { name: '查看目录（3）', exact: true }).click();
    const managedRow = page.locator('.provider-catalog-models li', { hasText: 'catalog-model-managed' });
    const freshRow = page.locator('.provider-catalog-models li', { hasText: 'catalog-model-fresh' });
    const unknownRow = page.locator('.provider-catalog-models li', { hasText: 'catalog-model-unknown' });
    await managedRow.getByRole('button', { name: '查看路由', exact: true }).waitFor();
    await freshRow.getByRole('button', { name: '添加路由', exact: true }).waitFor();
    await unknownRow.getByText('暂不支持为此协议添加托管路由', { exact: true }).waitFor();
    assert.equal(await unknownRow.getByRole('button').count(), 0, 'an unknown protocol cannot be coerced into an OpenAI route');

    await freshRow.getByRole('button', { name: '添加路由', exact: true }).click();
    await page.waitForFunction(() => document.getElementById('last-action')?.textContent?.includes('catalog-model-fresh'));
    const create = JSON.parse(await page.locator('#last-action').textContent() ?? '{}');
    assert.deepEqual(create, {
      kind: 'create',
      model: { id: 'catalog-model-fresh', protocol: 'anthropic', context_window: null, reservation_token_bound: null, reservation_bound_source: null },
      protocol: 'anthropic',
    });

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

test('catalog route actions stay disabled after a failed route read and recover on retry', { timeout: 60_000 }, async () => {
  const { server, browser, page, url } = await openFixture();
  try {
    let routeReads = 0;
    await page.route('**/internal/v1/upstreams/browse-account/models**', async route => route.fulfill({ json: {
      account_id: 'browse-account', status: 'ready', credential_generation: 1,
      last_attempt_at: 1_800_000_000_000, last_success_at: 1_800_000_000_000, expires_at: 1_800_086_400_000,
      error_code: null,
      models: [{ id: 'catalog-model-fresh', protocol: 'openai', context_window: null, reservation_token_bound: null, reservation_bound_source: null }],
      disabled_models: [],
    } }));
    await page.route('**/internal/v1/model-routes**', async route => {
      routeReads += 1;
      if (routeReads <= 4) return route.fulfill({ status: 503, json: { error: { code: 'unavailable', message: 'route list unavailable' } } });
      return route.fulfill({ json: [] });
    });
    await page.goto(url);
    await page.getByRole('button', { name: '查看目录（1）', exact: true }).click();
    const add = page.getByRole('button', { name: '添加路由', exact: true });
    await page.getByRole('button', { name: '重试读取路由', exact: true }).waitFor();
    assert.equal(await add.isDisabled(), true, 'a failed route list is not evidence that the model is uncovered');
    await page.getByRole('button', { name: '重试读取路由', exact: true }).click();
    await page.waitForFunction(() => {
      const button = [...document.querySelectorAll<HTMLButtonElement>('button')].find((candidate) => candidate.textContent?.trim() === '添加路由');
      return Boolean(button && !button.disabled);
    });
    assert.equal(routeReads, 5, 'the first read exhausts bounded automatic retries before the explicit retry succeeds');
  } finally {
    await browser.close();
    await server.close();
  }
});

test('managed sync invalidates an opened catalog route cache before offering add or view', { timeout: 60_000 }, async () => {
  const { server, browser, page, url } = await openFixture();
  try {
    let routeReads = 0;
    await page.route('**/internal/v1/upstreams/browse-account/models**', async route => route.fulfill({ json: {
      account_id: 'browse-account', status: 'ready', credential_generation: 1,
      last_attempt_at: 1_800_000_000_000, last_success_at: 1_800_000_000_000, expires_at: 1_800_086_400_000,
      error_code: null,
      models: [{ id: 'catalog-model-fresh', protocol: 'openai', context_window: null, reservation_token_bound: null, reservation_bound_source: null }],
      disabled_models: [],
    } }));
    await page.route('**/internal/v1/model-routes**', async route => {
      routeReads += 1;
      return route.fulfill({ json: routeReads === 1 ? [] : [{
        id: 'route-after-sync', tenant_external_id: 'fixture-a', public_model: 'catalog-model-fresh',
        upstream_account_ids: ['browse-account'], candidate_upstream_account_ids: ['browse-account'],
        upstream_model: 'catalog-model-fresh', protocol: 'openai', priority: 0, enabled: true,
        created_at: 1_800_000_000_000, updated_at: 1_800_000_000_000, grant_revision: 0,
      }] });
    });
    await page.route('**/internal/v1/upstreams/managed-account/models/sync-routes**', async route => route.fulfill({ json: syncResponse() }));
    await page.goto(url);
    await page.getByRole('button', { name: '查看目录（1）', exact: true }).click();
    await page.getByRole('button', { name: '添加路由', exact: true }).waitFor();
    await page.getByRole('button', { name: '同步模型', exact: true }).click();
    await page.getByRole('button', { name: '查看路由', exact: true }).waitFor();
    assert.equal(routeReads, 2, 'a successful managed sync reloads the already-open route cache');
  } finally {
    await browser.close();
    await server.close();
  }
});

test('catalog add performs a real cross-page handoff without expanding route authorization', { timeout: 60_000 }, async () => {
  const { server, browser, page, url } = await openFixture();
  try {
    await stubBrowseCatalog(page);
    await page.goto(`${url}?handoff=1`);
    await page.evaluate(() => sessionStorage.setItem('mtc-route-focus-v1', JSON.stringify({ tenant: 'fixture-a', routeId: 'stale-focus' })));
    await page.getByRole('button', { name: '查看目录（3）', exact: true }).click();
    await page.locator('.provider-catalog-models li', { hasText: 'catalog-model-fresh' }).getByRole('button', { name: '添加路由', exact: true }).click();
    await page.waitForURL('**/e2e/fixtures/managed-model-handoff.html');
    const workspace = page.getByRole('region', { name: '创建模型路由', exact: true });
    await workspace.getByText('已根据目录模型预填草稿，确认后即可创建。', { exact: true }).waitFor();
    assert.equal(await workspace.getByLabel('公开模型 · 必填', { exact: true }).inputValue(), 'catalog-model-fresh');
    assert.equal(await workspace.getByLabel('上游模型', { exact: true }).inputValue(), 'catalog-model-fresh');
    await workspace.getByText('Browse account', { exact: true }).waitFor();
    assert.equal(await page.evaluate(() => sessionStorage.getItem('mtc-route-focus-v1')), null, 'a create handoff replaces stale route focus');
    await page.waitForFunction(() => {
      const button = [...document.querySelectorAll<HTMLButtonElement>('button')].find((candidate) => candidate.textContent?.trim() === '创建路由');
      return Boolean(button && !button.disabled);
    });
    await page.getByRole('button', { name: '创建路由', exact: true }).click();
    await page.waitForFunction(() => window.managedHandoffPayloads.length === 1);
    const payload = await page.evaluate(() => window.managedHandoffPayloads[0]);
    assert.equal(payload.public_model, 'catalog-model-fresh');
    assert.equal(payload.upstream_model, 'catalog-model-fresh');
    assert.deepEqual(payload.upstream_account_ids, ['browse-account']);
    assert.deepEqual(payload.included_provider_group_ids, []);
    assert.deepEqual(payload.excluded_provider_group_ids, []);
    assert.deepEqual(payload.route_group_ids, []);
    assert.deepEqual(payload.granted_credential_ids, []);
    assert.equal(payload.protocol, 'anthropic');
    await page.getByText('路由已创建', { exact: true }).waitFor();
    await page.getByRole('button', { name: '创建模型路由', exact: true }).click();
    const reopenedWorkspace = page.getByRole('region', { name: '创建模型路由', exact: true });
    await reopenedWorkspace.waitFor();
    assert.equal(await reopenedWorkspace.getByText('路由已创建', { exact: true }).count(), 0, 'opening a new blank draft clears the previous creation result');
  } finally {
    await browser.close();
    await server.close();
  }
});

test('catalog view handoff finds and focuses a route on the second cursor page', { timeout: 60_000 }, async () => {
  const { server, browser, page, url } = await openFixture();
  try {
    await page.route('**/internal/v1/upstreams/browse-account/models**', async route => route.fulfill({ json: {
      account_id: 'browse-account', status: 'ready', credential_generation: 1,
      last_attempt_at: 1_800_000_000_000, last_success_at: 1_800_000_000_000, expires_at: 1_800_086_400_000,
      error_code: null,
      models: [{ id: 'catalog-model-paged', protocol: 'openai', context_window: null, reservation_token_bound: null, reservation_bound_source: null }],
      disabled_models: [],
    } }));
    await page.route('**/internal/v1/model-routes**', async route => {
      if (route.request().method() !== 'GET') return route.fallback();
      const requestUrl = new URL(route.request().url());
      if (requestUrl.searchParams.has('before_created_at')) {
        return route.fulfill({ json: [pagedRoute(pagedFocusRouteId, 1_800_000_000_000, 'catalog-model-paged')] });
      }
      return route.fulfill({ json: Array.from({ length: 100 }, (_, index) => pagedRoute(
        `00000000-0000-0000-0000-${String(index + 1).padStart(12, '0')}`,
        1_800_000_000_100 - index,
      )) });
    });
    await page.goto(`${url}?handoff=paged-focus`);
    await page.getByRole('button', { name: '查看目录（1）', exact: true }).click();
    await page.getByRole('button', { name: '查看路由', exact: true }).click();
    await page.waitForURL('**/e2e/fixtures/managed-model-handoff.html?paged-focus=1');

    const focusedRoute = page.locator(`[data-route-id="${pagedFocusRouteId}"]`);
    await focusedRoute.waitFor();
    assert.equal(await focusedRoute.evaluate((element) => document.activeElement === element), true, 'the route selected from the catalog receives keyboard focus');
    assert.equal(await focusedRoute.getAttribute('class'), 'route-focus');
    const reads = await page.evaluate(() => window.managedHandoffRouteReads);
    assert.equal(reads.length, 2, 'a focused handoff reads only as far as the target page');
    const first = new URLSearchParams(reads[0]);
    const second = new URLSearchParams(reads[1]);
    assert.equal(first.get('tenant_external_id'), 'fixture-a');
    assert.equal(first.get('limit'), '100');
    assert.equal(first.has('before_id'), false);
    assert.equal(second.get('tenant_external_id'), 'fixture-a');
    assert.equal(second.get('limit'), '100');
    assert.equal(second.get('before_created_at'), '1800000000001');
    assert.equal(second.get('before_id'), '00000000-0000-0000-0000-000000000100');
  } finally {
    await browser.close();
    await server.close();
  }
});

test('route handoff storage survives scope mismatch and cannot prefill a foreign account', { timeout: 60_000 }, async () => {
  const { server, browser, url } = await openFixture();
  try {
    const mismatch = await browser.newPage();
    await mismatch.addInitScript(() => {
      localStorage.setItem('mtc-locale', 'zh-CN');
      sessionStorage.setItem('mtc-route-draft-prefill-v1', JSON.stringify({ tenant: 'fixture-b', accountId: 'foreign-account', upstreamModel: 'catalog-model-fresh', publicModel: 'catalog-model-fresh', protocol: 'anthropic' }));
    });
    await mismatch.goto(new URL('/e2e/fixtures/managed-model-handoff.html', url).href);
    assert.notEqual(await mismatch.evaluate(() => sessionStorage.getItem('mtc-route-draft-prefill-v1')), null, 'a different tenant cannot consume or delete the handoff');
    await mismatch.evaluate(() => sessionStorage.setItem('mtc-route-draft-prefill-v1', JSON.stringify({ tenant: 'fixture-a', accountId: 42 })));
    await mismatch.reload();
    assert.notEqual(await mismatch.evaluate(() => sessionStorage.getItem('mtc-route-draft-prefill-v1')), null, 'a malformed handoff remains available for diagnosis instead of being deleted before validation');
    await mismatch.close();

    const foreign = await browser.newPage();
    await foreign.addInitScript(() => {
      localStorage.setItem('mtc-locale', 'zh-CN');
      sessionStorage.setItem('mtc-route-draft-prefill-v1', JSON.stringify({ tenant: 'fixture-a', accountId: 'foreign-account', upstreamModel: 'catalog-model-fresh', publicModel: 'catalog-model-fresh', protocol: 'anthropic' }));
    });
    await foreign.goto(new URL('/e2e/fixtures/managed-model-handoff.html', url).href);
    assert.equal(await foreign.getByText('已从目录模型预填草稿，请确认后创建。', { exact: true }).count(), 0);
    assert.equal(await foreign.getByRole('button', { name: '创建模型路由', exact: true }).getAttribute('aria-expanded'), 'false');
    assert.equal(await foreign.evaluate(() => window.managedHandoffPayloads.length), 0);
    await foreign.close();
  } finally {
    await browser.close();
    await server.close();
  }
});
