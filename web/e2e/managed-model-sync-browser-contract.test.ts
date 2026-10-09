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

    const sync = page.locator('.managed-model-sync-row').getByRole('button');
    await sync.waitFor({ state: 'visible' });
    await sync.click();
    await page.getByText('模型目录与路由已同步', { exact: true }).waitFor();
    await page.getByText('目录 2 个模型 · 新增 1 · 停用 0 · 恢复 0 · 保留 1 · 跳过 0', { exact: true }).waitFor();
    await page.getByText('1 个价格未匹配 · 3 个待确认 · 更新 7 · 保留 2 · 未匹配 1 · 待确认 3', { exact: true }).waitFor();
    await page.getByText('本次未能读取价格源：litellm。可稍后重新同步，或检查现有价格。', { exact: true }).waitFor();
    assert.deepEqual(syncTenants, ['fixture-a']);
    assert.equal(legacyPriceSyncRequests, 0, 'managed sync uses the combined server-side price result');

    warnings = ['sync_in_progress', 'operator_route_preserved', 'complete_catalog_unsupported'];
    await sync.click();
    await page.getByText('模型目录与路由需要检查', { exact: true }).waitFor();
    await page.getByText('另一次同步正在进行，本次跳过路由核对，请稍后重试。', { exact: true }).waitFor();
    await page.getByText('部分路由由管理员手动管理，已保持原样。', { exact: true }).waitFor();
    await page.getByText('此接入方式无法确认目录完整性，本次跳过路由核对。', { exact: true }).waitFor();

    warnings = [];
    legacyPricing = true;
    await sync.click();
    await page.getByText('本次未同步价格，模型目录与路由结果不受影响。', { exact: true }).waitFor();

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


test('managed price review actions are scoped and retries retain prior results without duplicate posts', { timeout: 60_000 }, async () => {
  const { server, browser, page, url } = await openFixture();
  try {
    await stubBrowseCatalog(page);
    let posts = 0;
    let releaseRetry = () => {};
    await page.route('**/internal/v1/upstreams/managed-account/models/sync-routes**', async route => {
      posts += 1;
      if (posts === 1) {
        const response = syncResponse({ models: 68 });
        response.price_sync = { status: 'partial', currency: 'USD', imported: 17, preserved: 2, unmatched: 49, ambiguous: 0, failed_sources: ['models.dev', 'litellm'], error_code: null };
        return route.fulfill({ json: response });
      }
      await new Promise<void>(resolve => { releaseRetry = resolve; });
      return route.fulfill({ status: 502, json: { error: { code: 'unknown_vendor_error', message: 'private payload must not appear' } } });
    });
    await page.goto(url);
    await page.getByRole('button', { name: '同步模型', exact: true }).click();
    await page.getByText('模型目录与路由已同步', { exact: true }).waitFor();
    assert.equal(await page.locator('.managed-model-sync-row button').count(), 1);
    assert.equal(await page.getByRole('button', { name: '同步模型', exact: true }).count(), 0);
    assert.equal(await page.getByRole('button', { name: '重新同步模型', exact: true }).count(), 1);
    await page.getByText('49 个价格未匹配 · 更新 17 · 保留 2 · 未匹配 49 · 待确认 0', { exact: true }).waitFor();
    await page.getByText('原有价格已保留，未被覆盖。', { exact: true }).waitFor();
    await page.getByText('未匹配表示本次没找到新价格', { exact: false }).waitFor();
    await page.evaluate(() => sessionStorage.setItem('mtc-route-draft-prefill-v1', 'existing-draft'));
    await page.getByRole('button', { name: '查看模型与路由', exact: true }).click();
    assert.deepEqual(JSON.parse(await page.locator('#last-action').textContent() ?? '{}'), { kind: 'models', accountId: 'managed-account', tenant: 'fixture-a' });
    await page.getByRole('button', { name: '检查模型价格', exact: true }).click();
    assert.deepEqual(JSON.parse(await page.locator('#last-action').textContent() ?? '{}'), { kind: 'pricing', accountId: 'managed-account', tenant: 'fixture-a' });
    assert.equal(await page.evaluate(() => sessionStorage.getItem('mtc-route-draft-prefill-v1')), 'existing-draft', 'review actions do not overwrite route draft handoffs');
    await page.evaluate(() => {
      const retry = [...document.querySelectorAll<HTMLButtonElement>('button')].find(button => button.textContent?.trim() === '重新同步模型');
      retry?.click(); retry?.click();
    });
    await page.getByText('正在同步模型与路由…', { exact: true }).waitFor();
    assert.equal(posts, 2);
    assert.equal(await page.locator('.managed-model-sync-row button').count(), 1);
    assert.equal(await page.getByRole('button', { name: '同步中…', exact: true }).isDisabled(), true);
    assert.equal(await page.getByRole('button', { name: '重新同步模型', exact: true }).count(), 0);
    assert.equal(await page.getByText('目录 68 个模型', { exact: false }).count(), 1);
    assert.equal(await page.getByRole('button', { name: '检查模型价格', exact: true }).isDisabled(), true);
    releaseRetry();
    await page.getByRole('alert').filter({ hasText: '未能取得本次同步结果' }).waitFor();
    assert.equal(await page.getByRole('button', { name: '重新同步模型', exact: true }).count(), 1);
    assert.equal(await page.getByRole('button', { name: '同步模型', exact: true }).count(), 0);
    assert.equal(await page.getByText('目录 68 个模型', { exact: false }).count(), 1);
    assert.equal(await page.getByText('unknown_vendor_error', { exact: false }).count(), 0);
    assert.equal(await page.getByText('private payload', { exact: false }).count(), 0);
    await page.getByRole('button', { name: '切换租户', exact: true }).click();
    assert.equal(await page.getByRole('button', { name: '检查模型价格', exact: true }).count(), 0);
    assert.equal(await page.getByText('目录 68 个模型', { exact: false }).count(), 0);
  } finally {
    await browser.close();
    await server.close();
  }
});

test('success, unmatched, error, and deferred states offer only useful actions', { timeout: 60_000 }, async () => {
  const { server, browser, page, url } = await openFixture();
  try {
    await stubBrowseCatalog(page);
    let price: ManagedModelSyncResponse['price_sync'] = { status: 'ready', currency: 'USD', imported: 2, preserved: 0, unmatched: 0, ambiguous: 0, failed_sources: [], error_code: null };
    await page.route('**/internal/v1/upstreams/managed-account/models/sync-routes**', route => route.fulfill({ json: { ...syncResponse(), price_sync: price } }));
    await page.goto(url);
    const sync = page.locator('.managed-model-sync-row').getByRole('button');
    await sync.click();
    await page.getByText('价格同步完成', { exact: false }).waitFor();
    assert.equal(await page.getByRole('button', { name: '检查模型价格', exact: true }).count(), 0);
    assert.equal(await page.getByRole('button', { name: '重新同步模型', exact: true }).count(), 0);
    price = { ...price, status: 'partial', unmatched: 49 };
    await sync.click();
    await page.getByRole('button', { name: '检查模型价格', exact: true }).waitFor();
    assert.equal(await page.getByRole('button', { name: '重新同步模型', exact: true }).count(), 0);
    price = { ...price, status: 'error', error_code: 'price_sync_failed' };
    await sync.click();
    await page.getByRole('alert').filter({ hasText: '价格同步遇到问题' }).waitFor();
    await page.getByRole('button', { name: '重新同步模型', exact: true }).waitFor();
    price = { status: 'deferred', currency: 'USD', imported: 0, preserved: 0, unmatched: 0, ambiguous: 0, failed_sources: [], error_code: 'managed_route_price_sync_deferred' };
    await sync.click();
    await page.getByText('本次未同步价格，模型目录与路由结果不受影响。', { exact: true }).waitFor();
    assert.equal(await page.getByRole('button', { name: '重新同步模型', exact: true }).count(), 0);
  } finally {
    await browser.close();
    await server.close();
  }
});


test('permission failures and unknown warnings show safe guidance without retrying automatically', { timeout: 60_000 }, async () => {
  const { server, browser, page, url } = await openFixture();
  try {
    await stubBrowseCatalog(page);
    let posts = 0;
    await page.route('**/internal/v1/upstreams/managed-account/models/sync-routes**', async route => {
      posts += 1;
      if (posts === 1) return route.fulfill({ json: syncResponse({ warnings: ['future_private_warning'] }) });
      return route.fulfill({ status: 403, json: { error: { code: 'private_permission_code', message: 'private permission payload' } } });
    });
    await page.goto(url);
    const sync = page.locator('.managed-model-sync-row').getByRole('button');
    await sync.click();
    await page.getByText('部分路由未完成核对', { exact: false }).waitFor();
    assert.equal(await page.getByText('future_private_warning', { exact: false }).count(), 0);
    await sync.click();
    await page.getByRole('alert').filter({ hasText: '当前凭据无法同步此账号' }).waitFor();
    assert.equal(await page.getByRole('button', { name: '重新同步模型', exact: true }).count(), 0);
    assert.equal(await page.getByText('private permission', { exact: false }).count(), 0);
    assert.equal(posts, 2);
    assert.equal(await sync.isEnabled(), true, 'a manual sync remains available after permissions are corrected outside this page');
    await sync.click();
    await page.getByRole('alert').filter({ hasText: '当前凭据无法同步此账号' }).waitFor();
    await page.waitForFunction(() => !document.querySelector<HTMLButtonElement>('.managed-model-sync-row button')?.disabled);
    assert.equal(posts, 3, 'only an explicit user action submits another request after denial');
    await page.goto(`${url}?readonly=1`);
    assert.equal(await sync.isDisabled(), true);
    assert.equal(posts, 3);
  } finally {
    await browser.close();
    await server.close();
  }
});

async function stubProvidersPage(page: Page, options: { denySync?: boolean; denyReads?: boolean } = {}) {
  const reads: URL[] = [];
  const posts: URL[] = [];
  const unexpectedWrites: string[] = [];
  await page.addInitScript(() => {
    localStorage.setItem('mtc.operator.service-credential.v1', 'fixture-token');
    localStorage.setItem('mtc.operator.tenant.v1', 'fixture-b');
  });
  await page.route('**/internal/v1/**', async route => {
    const request = route.request();
    const url = new URL(request.url());
    const path = url.pathname;
    if (request.method() === 'POST' && path === '/internal/v1/upstreams/managed-account/models/sync-routes') {
      posts.push(url);
      if (options.denySync && posts.length > 1) return route.fulfill({ status: 403, json: { error: { message: 'write denied' } } });
      return route.fulfill({ json: syncResponse({ models: 68 }) });
    }
    if (request.method() !== 'GET') {
      unexpectedWrites.push(path);
      return route.fulfill({ status: 403, json: { error: { message: 'write denied' } } });
    }
    reads.push(url);
    if (path === '/internal/v1/tenants') return route.fulfill({ json: [
      { external_id: 'fixture-a', name: 'Tenant A' }, { external_id: 'fixture-b', name: 'Tenant B' },
    ] });
    if (path === '/internal/v1/provider-types') return route.fulfill({ json: [{
      id: 'http-json', display_name: 'Fixture provider', source: 'builtin', protocols: ['openai'], modalities: ['text'],
      config_schema: { type: 'object', properties: {} }, credential_schema: { type: 'object', properties: {} },
    }] });
    if (path === '/internal/v1/upstreams') return route.fulfill({ json: [{
      id: 'managed-account', tenant_id: 'fixture-b', tenant_external_id: 'fixture-b', name: 'Managed account',
      driver: 'http-json', auth_kind: 'api_key', connection_method: 'api_key', credential_generation: 1,
      status: 'active', credential_expires_at: null, can_refresh: false, can_rotate: false, can_reauthorize: false,
      route_count: 68, config: {}, created_at: 1, updated_at: 2,
    }] });
    if (path === '/internal/v1/upstreams/managed-account/models') {
      if (options.denyReads) return route.fulfill({ status: 403, json: { error: { message: 'No permission to read this model catalog.' } } });
      return route.fulfill({ json: syncResponse({ models: 68 }).catalog });
    }
    if (path === '/internal/v1/model-prices/usage-summary') return route.fulfill({ json: { models: [] } });
    if (path === '/internal/v1/model-prices' && options.denyReads) return route.fulfill({ status: 403, json: { error: { message: 'price read denied' } } });
    if (path === '/internal/v1/transport-proxy-groups/access') return route.fulfill({ json: { can_manage: false } });
    if (path === '/internal/v1/monitoring-snapshot') return route.fulfill({ json: { top_upstream_models: [] } });
    if (path === '/internal/v1/upstream-availability') return route.fulfill({ json: { tenant_external_id: 'fixture-b', accounts: [] } });
    return route.fulfill({ json: [] });
  });
  return { reads, posts, unexpectedWrites };
}

test('full ProvidersPage review buttons open account details and the real pricing route in the selected tenant', { timeout: 60_000 }, async () => {
  const { server, browser, page, url } = await openFixture();
  try {
    const { reads, posts, unexpectedWrites } = await stubProvidersPage(page, { denySync: true });
    const pageErrors: string[] = [];
    page.on('pageerror', error => pageErrors.push(error.message));
    await page.goto(`${url}?full-page=1`);
    const account = page.locator('[data-upstream-id="managed-account"]');
    const sync = account.locator('.managed-model-sync-row').getByRole('button');
    await sync.click();
    await account.getByText('模型目录与路由已同步', { exact: true }).waitFor();
    assert.equal(await account.getByRole('button', { name: '重新同步模型', exact: true }).count(), 1);
    assert.equal(await account.getByRole('button', { name: '同步模型', exact: true }).count(), 0);
    await sync.click();
    await account.getByRole('alert').filter({ hasText: '当前凭据无法同步此账号' }).waitFor();
    assert.equal(await sync.isEnabled(), true, 'manual retry remains possible after an external permission correction');
    await page.evaluate(() => sessionStorage.setItem('mtc-route-draft-prefill-v1', 'existing-draft'));
    const models = account.getByRole('button', { name: '查看模型与路由', exact: true });
    assert.equal(await models.isEnabled(), true, 'read navigation stays available after write denial');
    await models.click();
    const detail = page.getByRole('region', { name: 'Managed account · 管理账号', exact: true });
    assert.equal(await detail.getAttribute('id'), 'provider-details-managed-account');
    assert.equal(await account.isVisible(), false, 'catalog navigation replaces the account directory');
    await Promise.all([
      page.waitForResponse(response => new URL(response.url()).pathname === '/internal/v1/model-routes'),
      detail.getByRole('button', { name: '查看目录（68）', exact: true }).click(),
    ]);
    await detail.getByText('catalog-model-0', { exact: true }).waitFor();
    await detail.getByRole('button', { name: '返回账号列表', exact: true }).click();
    assert.equal(await account.locator('[data-manage-account-trigger]').evaluate(element => element === document.activeElement), true);
    const modelReads = reads.filter(value => value.pathname === '/internal/v1/upstreams/managed-account/models' || value.pathname === '/internal/v1/model-routes');
    assert.equal(modelReads.length, 2);
    assert.ok(modelReads.every(value => value.searchParams.get('tenant_external_id') === 'fixture-b'));
    await Promise.all([
      page.waitForResponse(response => new URL(response.url()).pathname === '/internal/v1/model-prices/usage-summary'),
      account.getByRole('button', { name: '检查模型价格', exact: true }).click(),
    ]);
    await page.waitForURL('**/operator?view=pricing');
    await page.locator('#operator-panel-pricing .pricing-page').waitFor();
    await page.getByText('0 个已使用模型', { exact: true }).waitFor();
    assert.equal(await page.locator('.tenant-picker select').inputValue(), 'fixture-b');
    assert.equal(await page.evaluate(() => localStorage.getItem('mtc.operator.tenant.v1')), 'fixture-b');
    const usage = reads.filter(value => value.pathname === '/internal/v1/model-prices/usage-summary');
    assert.ok(usage.length > 0);
    assert.ok(usage.every(value => value.searchParams.get('tenant_external_id') === 'fixture-b'));
    assert.ok(reads.some(value => value.pathname === '/internal/v1/model-prices'));
    assert.equal(new URL(page.url()).searchParams.toString(), 'view=pricing', 'review uses the existing route without unsupported filters');
    assert.equal(await page.evaluate(() => sessionStorage.getItem('mtc-route-draft-prefill-v1')), 'existing-draft');
    assert.equal(posts.length, 2, 'review actions never post another sync');
    assert.ok(posts.every(value => value.searchParams.get('tenant_external_id') === 'fixture-b'));
    assert.deepEqual(unexpectedWrites, []);
    assert.deepEqual(pageErrors, []);
  } finally {
    await browser.close();
    await server.close();
  }
});

test('full ProvidersPage read denial uses existing catalog and pricing error states', { timeout: 60_000 }, async () => {
  const { server, browser, page, url } = await openFixture();
  try {
    const { reads, posts, unexpectedWrites } = await stubProvidersPage(page, { denyReads: true });
    await page.goto(`${url}?full-page=1`);
    const account = page.locator('[data-upstream-id="managed-account"]');
    await account.getByRole('button', { name: '同步模型', exact: true }).click();
    await account.getByRole('button', { name: '查看模型与路由', exact: true }).click();
    const detail = page.getByRole('region', { name: 'Managed account · 管理账号', exact: true });
    assert.equal(await detail.getAttribute('id'), 'provider-details-managed-account');
    await detail.getByRole('alert').filter({ hasText: 'No permission to read this model catalog.' }).waitFor();
    await detail.getByRole('button', { name: '返回账号列表', exact: true }).click();
    assert.equal(await detail.count(), 0);
    assert.equal(await account.locator('[data-manage-account-trigger]').evaluate(element => element === document.activeElement), true);
    await account.getByRole('button', { name: '检查模型价格', exact: true }).click();
    await page.waitForURL('**/operator?view=pricing');
    await page.getByRole('alert').filter({ hasText: '部分计费数据不可用，请重试。' }).waitFor();
    await page.locator('.pricing-table-caption').filter({ hasText: '价格目录未完整加载' }).waitFor();
    assert.equal(reads.filter(value => value.pathname === '/internal/v1/model-prices').length, 1, '403 read denial is not retried');
    assert.equal(await page.locator('.tenant-picker select').inputValue(), 'fixture-b');
    assert.equal(posts.length, 1);
    assert.deepEqual(unexpectedWrites, []);
  } finally {
    await browser.close();
    await server.close();
  }
});
