import { expect, request as playwrightRequest, test, type APIRequestContext, type Page } from '@playwright/test';

const bootstrapToken = process.env.MTC_E2E_SERVICE_TOKEN ?? 'browser-e2e-bootstrap-not-a-real-token';
const mockPort = Number(process.env.MTC_E2E_MOCK_PORT ?? 41740);
const tenant = 'browser-e2e-tenant';
const model = 'browser-e2e-model';

interface SeedState {
  clientCredential: string;
  clientKeyId: string;
  serviceCredential: string;
  upstreamId: string;
  routeId: string;
}

let seed: SeedState;

async function json<T>(
  api: APIRequestContext,
  method: 'get' | 'post',
  path: string,
  credential: string,
  data?: unknown,
): Promise<T> {
  const response = await api[method](path, {
    headers: { Authorization: `Bearer ${credential}` },
    data,
  });
  const text = await response.text();
  expect(response.ok(), `${method.toUpperCase()} ${path}: ${response.status()} ${text}`).toBeTruthy();
  return JSON.parse(text) as T;
}

async function seedThroughHttp(api: APIRequestContext): Promise<SeedState> {
  const upstream = await json<{ id: string }>(api, 'post', '/internal/v1/upstreams', bootstrapToken, {
    tenant_external_id: tenant,
    name: 'Browser mock upstream',
    driver: 'http-json',
    config: { base_url: `http://127.0.0.1:${mockPort}` },
    credential: { type: 'api_key', value: 'browser-mock-upstream-not-a-secret' },
  });
  const route = await json<{ id: string }>(api, 'post', '/internal/v1/model-routes', bootstrapToken, {
    tenant_external_id: tenant,
    public_model: model,
    upstream_account_id: upstream.id,
    upstream_model: 'mock-provider-model',
    protocol: 'openai',
    priority: 0,
  });
  await json(api, 'post', `/internal/v1/prices/USD/${model}`, bootstrapToken, {
    input_per_million: '1',
    output_per_million: '2',
  });
  const client = await json<{ key: string; key_id: string }>(api, 'post', '/internal/v1/keys', bootstrapToken, {
    tenant_external_id: tenant,
    principal_external_id: 'browser-e2e-user',
    alias: 'Browser E2E credential',
    currency: 'USD',
    initial_balance: '10',
    policy: {
      allowed_models: [model],
      requests_per_minute: 1000,
      tokens_per_minute: 100000,
      max_concurrency: 4,
      daily_budget: null,
      weekly_budget: null,
      lifetime_budget: '10',
    },
  });
  const service = await json<{ token: string }>(api, 'post', '/internal/v1/service-tokens', bootstrapToken, {
    name: 'Browser E2E tenant operator',
    tenant_external_id: tenant,
    scopes: [
      'requests:read', 'providers:read', 'providers:write', 'plugins:read', 'schemas:read',
      'keys:read', 'keys:write', 'routes:read', 'routes:write', 'prices:read', 'oauth:write',
    ],
  });
  await json(api, 'post', '/v1/chat/completions', client.key, {
    model,
    messages: [{ role: 'user', content: 'Create one observable browser test request.' }],
    max_tokens: 32,
  });
  for (let batchStart = 0; batchStart < 49; batchStart += 2) {
    await Promise.all(Array.from({ length: Math.min(2, 49 - batchStart) }, (_, offset) => json(api, 'post', '/v1/chat/completions', client.key, {
      model,
      messages: [{ role: 'user', content: `Create observable browser test request ${batchStart + offset + 2}.` }],
      max_tokens: 32,
    })));
  }
  const failed = await api.post('/v1/chat/completions', {
    headers: { Authorization: `Bearer ${client.key}` },
    data: {
      model,
      messages: [{ role: 'user', content: 'force observable error' }],
      max_tokens: 32,
    },
  });
  expect(failed.status()).toBe(429);

  await expect.poll(async () => {
    const stats = await json<{
      summary: { total_requests: number; successful_requests: number; failed_requests: number };
    }>(api, 'get', '/self/v1/stats', client.key);
    return [
      stats.summary.total_requests,
      stats.summary.successful_requests,
      stats.summary.failed_requests,
    ];
  }, { timeout: 30_000 }).toEqual([51, 50, 1]);
  return {
    clientCredential: client.key,
    clientKeyId: client.key_id,
    serviceCredential: service.token,
    upstreamId: upstream.id,
    routeId: route.id,
  };
}

function observePage(page: Page) {
  const consoleErrors: string[] = [];
  const failedRequests: string[] = [];
  page.on('console', (message) => {
    if (message.type() === 'error') consoleErrors.push(message.text());
  });
  page.on('pageerror', (error) => consoleErrors.push(error.message));
  page.on('requestfailed', (request) => {
    const failure = request.failure()?.errorText ?? 'unknown';
    // React intentionally aborts the long-lived SSE tail when tenant or tab state changes.
    // Chromium reports that lifecycle cancellation as requestfailed even though the network is healthy.
    if (request.url().includes('/internal/v1/request-events') && failure.includes('ERR_ABORTED')) return;
    failedRequests.push(`${request.method()} ${request.url()}: ${failure}`);
  });
  return { consoleErrors, failedRequests };
}

async function assertNoHorizontalOverflow(page: Page) {
  const layout = await page.evaluate(() => {
    const viewport = document.documentElement.clientWidth;
    const overflowers = Array.from(document.querySelectorAll<HTMLElement>('body *')).map((element) => {
      const bounds = element.getBoundingClientRect();
      return {
        element: `${element.tagName.toLowerCase()}${element.id ? `#${element.id}` : ''}${Array.from(element.classList).map((name) => `.${name}`).join('')}`,
        left: Math.round(bounds.left),
        right: Math.round(bounds.right),
        width: Math.round(bounds.width),
        scrollWidth: element.scrollWidth,
      };
    }).filter((item) => item.left < 0 || item.right > viewport || item.width > viewport).slice(0, 12);
    return {
      viewport,
      innerWidth: window.innerWidth,
      visualViewport: window.visualViewport?.width,
      mobile480: window.matchMedia('(width <= 480px)').matches,
      mobile900: window.matchMedia('(width <= 900px)').matches,
      document: document.documentElement.scrollWidth,
      overflowers,
    };
  });
  expect(layout.document, JSON.stringify(layout, null, 2)).toBeLessThanOrEqual(layout.viewport);
}

test.describe.configure({ mode: 'serial' });

test.beforeAll(async ({ baseURL }) => {
  const api = await playwrightRequest.newContext({ baseURL });
  try {
    seed = await seedThroughHttp(api);
  } finally {
    await api.dispose();
  }
});

test('operator dogfooding covers tenant isolation, unified providers, pricing, i18n, themes, and favicon', async ({ page, request }) => {
  const observed = observePage(page);
  await page.addInitScript(() => {
    localStorage.setItem('mtc-theme', 'dark');
    localStorage.setItem('mtc-locale', 'zh-CN');
    sessionStorage.clear();
  });
  await page.goto('/operator');

  const favicon = await page.locator('link[rel="icon"]').getAttribute('href');
  expect(favicon).toBe('/ui-assets/token-center-icon-32.png');
  const faviconResponse = await request.get(favicon!);
  expect(faviconResponse.status()).toBe(200);
  expect(faviconResponse.headers()['content-type']).toContain('image/png');
  expect((await faviconResponse.body()).byteLength).toBeGreaterThan(100);

  await page.locator('input[type="password"]').fill(seed.serviceCredential);
  await page.getByRole('button', { name: '连接' }).click();
  await expect(page.locator('.metric').filter({ hasText: '总请求' }).locator('strong')).toHaveText('51');
  await expect(page.locator('.notice.error')).toHaveCount(0);
  await expect(page.locator('.tenant-picker select')).toContainText(tenant);
  await page.locator('.tenant-picker select').selectOption(tenant);

  const trafficFilters = page.locator('.traffic-filters');
  await trafficFilters.getByLabel('凭据别名前缀').fill('Browser');
  await trafficFilters.getByLabel('用户主体前缀').fill('browser-e2e');
  await trafficFilters.getByLabel('路由主键').fill(seed.routeId);
  await trafficFilters.getByLabel('上游提供商').selectOption(seed.upstreamId);
  await trafficFilters.getByLabel('最低费用').fill('0');
  await trafficFilters.getByLabel('最高费用').fill('1');
  await trafficFilters.getByRole('button', { name: '应用筛选' }).click();
  await expect(page.locator('.metric').filter({ hasText: '总请求' }).locator('strong')).toHaveText('51');
  await page.getByRole('button', { name: '按 http_429 筛选请求' }).click();
  await expect(trafficFilters.getByLabel('状态')).toHaveValue('error');
  await expect(trafficFilters.getByLabel('错误码')).toHaveValue('http_429');
  await expect(page.locator('.panel tbody tr')).toHaveCount(1);
  await expect(page.locator('.panel tbody')).toContainText('http_429');
  await trafficFilters.getByRole('button', { name: '清除筛选' }).click();
  await expect(page.locator('.metric').filter({ hasText: '总请求' }).locator('strong')).toHaveText('51');

  const crossTenant = await request.get('/internal/v1/stats?tenant_external_id=another-tenant', {
    headers: { Authorization: `Bearer ${seed.serviceCredential}` },
  });
  expect(crossTenant.status()).toBe(403);

  const malformedWithoutAuthentication = await request.post('/internal/v1/keys', {
    headers: { 'Content-Type': 'application/json' },
    data: Buffer.from('{malformed-json'),
  });
  expect(malformedWithoutAuthentication.status()).toBe(401);

  await page.getByRole('tab', { name: '上游提供商' }).click();
  const onboarding = page.locator('.provider-onboarding');
  await expect(onboarding.getByRole('button', { name: '直接凭据' })).toBeVisible();
  await expect(onboarding.getByRole('button', { name: 'OAuth / 订阅授权' })).toBeVisible();
  await expect(page.getByText('Browser mock upstream')).toBeVisible();
  const providerAccount = page.locator('.provider-account').filter({ hasText: 'Browser mock upstream' });
  await expect(providerAccount).toContainText('API 凭据');
  await expect(providerAccount).toContainText('1 条路由');
  await providerAccount.getByRole('button', { name: '健康检查' }).click();
  await expect(providerAccount).toContainText('连接正常');
  await providerAccount.getByRole('button', { name: '编辑' }).click();
  const upstreamEditor = page.locator('.inline-editor').filter({ hasText: '编辑 Browser mock upstream' });
  await upstreamEditor.getByLabel('上游名称').fill('Browser mock upstream edited');
  await upstreamEditor.getByRole('button', { name: '保存' }).click();
  await expect(page.getByRole('status')).toContainText('已更新 Browser mock upstream');
  await expect(providerAccount).toContainText('Browser mock upstream edited');
  await providerAccount.getByRole('button', { name: '停用' }).click();
  await expect(providerAccount).toContainText('已停用');
  await providerAccount.getByRole('button', { name: '启用' }).click();
  await expect(providerAccount).toContainText('正常');
  await onboarding.getByRole('button', { name: 'OAuth / 订阅授权' }).click();
  await expect(onboarding.locator('select').first()).toContainText('CPA Subscription Bridge');

  await page.getByRole('tab', { name: '模型路由' }).click();
  const routeRow = page.locator('tbody tr').filter({ hasText: model });
  await expect(routeRow).toContainText('Browser mock upstream');
  await routeRow.getByRole('button', { name: '编辑' }).click();
  const routeEditor = page.locator('.inline-editor');
  await routeEditor.getByLabel('上游模型').fill('mock-provider-model-v2');
  await routeEditor.getByRole('button', { name: '保存' }).click();
  await expect(page.getByRole('status')).toContainText('路由已更新');
  await expect(routeRow).toContainText('mock-provider-model-v2');
  await routeRow.getByRole('button', { name: '停用' }).click();
  await expect(routeRow).toContainText('已停用');
  await routeRow.getByRole('button', { name: '启用' }).click();
  await expect(routeRow).toContainText('已启用');

  await page.getByRole('tab', { name: '凭据管理' }).click();
  await expect(page.getByRole('heading', { name: '创建下游凭据' })).toBeVisible();
  await expect(page.getByRole('button', { name: '创建凭据' })).toBeVisible();
  await page.getByRole('button', { name: '创建凭据' }).click();
  await expect(page.locator('.schema-errors')).toContainText('请修正');
  await expect(page.locator('.schema-errors')).not.toContainText('is required');

  await page.getByRole('tab', { name: '模型计费' }).click();
  await expect(page.getByRole('heading', { name: '模型计费' })).toBeVisible();
  await expect(page.getByText(model)).toBeVisible();
  await expect(page.getByText('models.dev → LiteLLM → OpenRouter')).toBeVisible();

  const themeButton = page.locator('.rail').getByRole('button', { name: '切换到亮色主题' });
  await themeButton.click();
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'light');
  await expect(page.locator('meta[name="theme-color"]')).toHaveAttribute('content', '#f4f7f5');
  await page.locator('.rail .language-toggle').click();
  await expect(page.locator('html')).toHaveAttribute('lang', 'en');
  await expect(page.getByRole('tab', { name: 'Credential management' })).toBeVisible();
  await expect(page.getByRole('heading', { name: 'Model pricing' })).toBeVisible();

  await page.setViewportSize({ width: 375, height: 812 });
  await assertNoHorizontalOverflow(page);
  expect(observed.consoleErrors).toEqual([]);
  expect(observed.failedRequests).toEqual([]);
});

test('client credential portal shows non-empty requests and statistics without browser failures', async ({ page, request }) => {
  const observed = observePage(page);
  await page.addInitScript(() => {
    localStorage.setItem('mtc-theme', 'light');
    localStorage.setItem('mtc-locale', 'zh-CN');
    sessionStorage.clear();
  });
  await page.setViewportSize({ width: 375, height: 812 });
  await page.goto('/portal');
  await page.locator('input[type="password"]').fill(seed.clientCredential);
  await page.getByRole('button', { name: '载入' }).click();

  await expect(page.locator('.metric').filter({ hasText: '总请求' }).locator('strong')).toHaveText('51');
  await expect(page.locator('.metric').filter({ hasText: '成功' }).locator('strong')).toHaveText('50');
  await expect(page.locator('.metric').filter({ hasText: '失败' }).locator('strong')).toHaveText('1');
  await expect(page.getByRole('heading', { name: 'Browser E2E credential' })).toBeVisible();
  await expect(page.getByText(model).first()).toBeVisible();
  await expect(page.locator('.self-history tbody tr')).toHaveCount(50);
  await expect(page.getByRole('button', { name: '加载更早请求' })).toBeVisible();

  const filters = page.locator('.self-request-filters');
  await filters.getByLabel('上游主键').fill(seed.upstreamId);
  await filters.getByLabel('路由主键').fill(seed.routeId);
  await filters.getByLabel('最低费用').fill('0');
  await filters.getByLabel('最高费用').fill('1');
  await filters.getByRole('button', { name: '应用筛选' }).click();
  await expect(page.locator('.metric').filter({ hasText: '总请求' }).locator('strong')).toHaveText('51');
  await expect(page.locator('.self-history tbody tr')).toHaveCount(50);
  await page.getByRole('button', { name: '按 http_429 筛选请求' }).click();
  await expect(filters.getByLabel('状态')).toHaveValue('error');
  await expect(filters.getByLabel('错误码')).toHaveValue('http_429');
  await expect(page.locator('.self-history tbody tr')).toHaveCount(1);
  await expect(page.locator('.self-history')).toContainText('http_429');
  await page.locator('.self-history').getByRole('button', { name: '查看' }).click();
  const drawer = page.getByRole('dialog');
  await expect(drawer).toContainText('429');
  await expect(drawer).toContainText('mock observable rate limit');
  await drawer.getByRole('button', { name: '关闭' }).click();

  await filters.getByRole('button', { name: '清除筛选' }).click();
  await expect(page.locator('.self-history tbody tr')).toHaveCount(50);
  await page.getByRole('button', { name: '加载更早请求' }).click();
  await expect(page.locator('.self-history tbody tr')).toHaveCount(51);
  await expect(page.locator('.notice.error')).toHaveCount(0);
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'light');
  await assertNoHorizontalOverflow(page);

  const forbiddenManagementRead = await request.get('/internal/v1/tenants', {
    headers: { Authorization: `Bearer ${seed.clientCredential}` },
  });
  expect([401, 403]).toContain(forbiddenManagementRead.status());
  const cannotSelectAnotherCredential = await request.get(`/self/v1/stats?key_id=00000000-0000-0000-0000-000000000001`, {
    headers: { Authorization: `Bearer ${seed.clientCredential}` },
  });
  expect(cannotSelectAnotherCredential.status()).toBe(200);
  expect((await cannotSelectAnotherCredential.json()).key_id).toBe(seed.clientKeyId);
  expect(observed.consoleErrors).toEqual([]);
  expect(observed.failedRequests).toEqual([]);

  await page.locator('input[type="password"]').fill('invalid-browser-test-credential');
  await page.getByRole('button', { name: '载入' }).click();
  await expect(page.getByRole('alert')).toContainText('凭据无效或已失效');
  await page.locator('.mobile-controls .language-toggle').click();
  await page.getByRole('button', { name: 'Load' }).click();
  await expect(page.getByRole('alert')).toContainText('invalid or no longer active');
});
