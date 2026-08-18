import assert from 'node:assert/strict';
import { Given, Then, When } from '@cucumber/cucumber';
import type { Locator, Page } from 'playwright';
import { baseURL, eventually, model, runtime, tenant } from '../support/runtime.js';
import type { DogfoodWorld } from '../support/world.js';

interface RealtimeReconnectObservation {
  connectionUrls: string[];
  disconnectedForMs: number;
  finalCursorId: string;
  finalRowCount: number;
}
interface StrictUsageObservation {
  requestUrls: string[];
}

const realtimeReconnectObservations = new WeakMap<DogfoodWorld, RealtimeReconnectObservation>();
const strictUsageObservations = new WeakMap<DogfoodWorld, StrictUsageObservation>();
const uuidPattern = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;

Given('dogfood 服务已有隔离租户、统一上游、请求记录和多模态价格', function () {
  runtime.requireSeed();
});

When('管理员以中文暗色主题连接控制台', async function (this: DogfoodWorld) {
  await connectOperator(this, 'dark');
});

Then('控制台提供有效 favicon', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const href = await page.locator('link[rel="icon"]').getAttribute('href');
  assert.equal(href, '/ui-assets/token-center-icon-32.png');
  const response = await fetch(new URL(href, baseURL));
  assert.equal(response.status, 200);
  assert.match(response.headers.get('content-type') ?? '', /image\/png/);
  assert.ok((await response.arrayBuffer()).byteLength > 100);
});

Then('请求列表的完整筛选和错误下钻均可用', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const seed = runtime.requireSeed();

  await page.getByRole('tab', { name: '请求统计', exact: true }).click();
  await assertExactText(metric(page, '请求数'), '51');
  await assertNoCount(page.locator('.notice.error'));

  await page.getByRole('tab', { name: '实时请求', exact: true }).click();
  await assertVisible(page.getByRole('heading', { name: '实时请求尾流', exact: true }));
  await assertNoCount(page.locator('#operator-panel-traffic .metric'));
  const filters = page.locator('.traffic-filters');
  const protocolValues = await filters.getByLabel('协议').locator('option').evaluateAll((options) =>
    options.map((option) => (option as HTMLOptionElement).value));
  assert.deepEqual(protocolValues, ['', 'openai', 'anthropic', 'openai-image', 'generation']);
  await filters.getByLabel('凭据别名前缀').fill('Browser');
  await filters.getByLabel('用户主体前缀').fill('browser-e2e');
  await filters.getByLabel('路由主键').fill(seed.routeId);
  await filters.getByLabel('上游提供商').selectOption(seed.upstreamId);
  await filters.getByLabel('最低费用').fill('0');
  await filters.getByLabel('最高费用').fill('1000');
  await filters.getByRole('button', { name: '应用筛选' }).click();
  await assertCount(page.locator('#operator-panel-traffic tbody tr'), 51);

  await filters.getByLabel('状态').selectOption('error');
  await filters.getByLabel('错误码').fill('http_429');
  await filters.getByRole('button', { name: '应用筛选' }).click();
  await assertValue(filters.getByLabel('状态'), 'error');
  await assertValue(filters.getByLabel('错误码'), 'http_429');
  await assertCount(page.locator('#operator-panel-traffic tbody tr'), 1);
  await assertContains(page.locator('#operator-panel-traffic tbody'), 'http_429');

  await filters.getByRole('button', { name: '清除筛选' }).click();
  await assertCount(page.locator('#operator-panel-traffic tbody tr'), 51);
});

Then('租户边界和未认证请求在解析正文前生效', async function () {
  const seed = runtime.requireSeed();
  const crossTenant = await fetch(new URL('/internal/v1/stats?tenant_external_id=another-tenant', baseURL), {
    headers: { Authorization: `Bearer ${seed.serviceCredential}` },
  });
  assert.equal(crossTenant.status, 403);

  const malformedWithoutAuthentication = await fetch(new URL('/internal/v1/keys', baseURL), {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: '{malformed-json',
  });
  assert.equal(malformedWithoutAuthentication.status, 401);
});

When('管理员维护统一上游和模型路由', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const seed = runtime.requireSeed();

  await page.getByRole('tab', { name: '上游提供商', exact: true }).click();
  const onboarding = page.locator('.provider-onboarding');
  await assertVisible(onboarding.getByRole('button', { name: '直接凭据', exact: true }));
  await assertVisible(onboarding.getByRole('button', { name: 'OAuth / 订阅授权', exact: true }));
  await assertVisible(page.getByText('Browser mock upstream', { exact: true }));
  const providerAccount = page.locator('.provider-account').filter({ hasText: 'Browser mock upstream' });
  await assertContains(providerAccount, 'API 凭据');
  await assertContains(providerAccount, '1 条路由');
  await providerAccount.getByRole('button', { name: '健康检查' }).click();
  await assertContains(providerAccount, '连接正常');
  await providerAccount.getByRole('button', { name: '编辑', exact: true }).click();
  const upstreamEditor = page.locator('.inline-editor').filter({ hasText: '编辑 Browser mock upstream' });
  await upstreamEditor.getByLabel('上游名称').fill('Browser mock upstream edited');
  await upstreamEditor.getByRole('button', { name: '保存', exact: true }).click();
  await assertContains(page.getByRole('status'), '已更新 Browser mock upstream');
  await assertContains(providerAccount, 'Browser mock upstream edited');
  seed.upstreamName = 'Browser mock upstream edited';
  await providerAccount.getByRole('button', { name: '停用', exact: true }).click();
  await assertContains(providerAccount, '已停用');
  await providerAccount.getByRole('button', { name: '启用', exact: true }).click();
  await assertContains(providerAccount, '正常');
  await onboarding.getByRole('button', { name: 'OAuth / 订阅授权', exact: true }).click();
  await assertContains(onboarding.locator('select').first(), 'CPA Subscription Bridge');

  await page.getByRole('tab', { name: '模型路由', exact: true }).click();
  const routeRow = page.locator('tbody tr').filter({ hasText: model });
  await assertContains(routeRow, 'Browser mock upstream');
  await routeRow.getByRole('button', { name: '编辑', exact: true }).click();
  const routeEditor = page.locator('.inline-editor');
  await routeEditor.getByLabel('上游模型').fill('mock-provider-model-v2');
  await routeEditor.getByRole('button', { name: '保存', exact: true }).click();
  await assertContains(page.getByRole('status'), '路由已更新');
  await assertContains(routeRow, 'mock-provider-model-v2');
  await routeRow.getByRole('button', { name: '停用', exact: true }).click();
  await assertContains(routeRow, '已停用');
  await routeRow.getByRole('button', { name: '启用', exact: true }).click();
  await assertContains(routeRow, '已启用');
});

Then('下游凭据表单使用本地化校验且模型计费可见', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await page.getByRole('tab', { name: '凭据管理', exact: true }).click();
  await assertVisible(page.getByRole('heading', { name: '创建下游凭据', exact: true }));
  const createButton = page.getByRole('button', { name: '创建凭据', exact: true });
  await assertVisible(createButton);
  await createButton.click();
  await assertContains(page.locator('.schema-errors'), '请修正');
  await assertNotContains(page.locator('.schema-errors'), 'is required');

  await page.getByRole('tab', { name: '模型计费', exact: true }).click();
  await assertVisible(page.getByRole('heading', { name: '模型计费', exact: true }));
  await assertVisible(page.getByText(model, { exact: true }).first());
  await assertContains(page.locator('.pricing-summary'), 'models.dev → LiteLLM → OpenRouter');
});

Then('管理员可以重命名凭据并查看当前限制状态', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const seed = runtime.requireSeed();
  await page.getByRole('tab', { name: '凭据管理', exact: true }).click();
  const resource = page.locator('.managed-resource').filter({ hasText: seed.clientKeyId });
  await assertContains(resource, 'Browser E2E credential');

  await resource.getByRole('button', { name: '修改别名', exact: true }).click();
  const editor = resource.locator('.inline-editor').filter({ hasText: '修改' });
  await editor.locator('input').fill('Browser E2E renamed credential');
  const renameResponse = page.waitForResponse((response) =>
    response.url().endsWith(`/internal/v1/keys/${seed.clientKeyId}/alias`) &&
    response.request().method() === 'PATCH');
  await editor.getByRole('button', { name: '保存', exact: true }).click();
  const renamed = await renameResponse;
  assert.equal(renamed.status(), 200);
  assert.equal((await renamed.json()).key_id, seed.clientKeyId);
  await assertContains(resource, 'Browser E2E renamed credential');

  const limitsResponse = page.waitForResponse((response) =>
    response.url().endsWith(`/internal/v1/keys/${seed.clientKeyId}/limits`) &&
    response.request().method() === 'GET');
  await resource.getByRole('button', { name: '当前额度状态', exact: true }).click();
  const limits = await limitsResponse;
  assert.equal(limits.status(), 200);
  const snapshot = await limits.json();
  assert.equal(snapshot.key_id, seed.clientKeyId);
  assert.equal(snapshot.currency, 'USD');
  assert.ok(snapshot.rpm && snapshot.tpm && snapshot.concurrency);
  await assertVisible(resource.getByRole('heading', { name: '当前额度与限流状态', exact: true }));
  await assertContains(resource, 'RPM');
  await assertContains(resource, 'TPM');
  await assertContains(resource, '并发');
  await assertContains(resource, '每日额度');

  await resource.getByRole('button', { name: '修改别名', exact: true }).click();
  const restoreEditor = resource.locator('.inline-editor').filter({ hasText: '修改' });
  await restoreEditor.locator('input').fill('Browser E2E credential');
  const restoreResponse = page.waitForResponse((response) =>
    response.url().endsWith(`/internal/v1/keys/${seed.clientKeyId}/alias`) &&
    response.request().method() === 'PATCH');
  await restoreEditor.getByRole('button', { name: '保存', exact: true }).click();
  assert.equal((await restoreResponse).status(), 200);
  await assertContains(resource, 'Browser E2E credential');
  await page.getByRole('tab', { name: '模型计费', exact: true }).click();
});

When('管理员切换为亮色英文界面和手机视口', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await page.locator('.rail').getByRole('button', { name: '切换到亮色主题' }).click();
  await assertAttribute(page.locator('html'), 'data-theme', 'light');
  await assertAttribute(page.locator('meta[name="theme-color"]'), 'content', '#f4f7f5');
  await page.locator('.rail .language-toggle').click();
  await assertAttribute(page.locator('html'), 'lang', 'en');
  await page.setViewportSize({ width: 375, height: 812 });
});

Then('英文导航、主题色和响应式布局均正确且浏览器没有失败', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await assertVisible(page.getByRole('tab', { name: 'Credential management', exact: true }));
  await page.getByRole('tab', { name: 'Model pricing', exact: true }).click();
  await assertVisible(page.getByRole('heading', { name: 'Model pricing', exact: true }));
  await assertNoHorizontalOverflow(page);
  this.assertNoBrowserFailures();
});

When('管理员进入请求统计', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await page.getByRole('tab', { name: '请求统计', exact: true }).click();
  await assertVisible(page.getByRole('heading', { name: '用量分析', exact: true }));
  await assertExactText(metric(page, '请求数'), '51');
});

Then('总览、趋势、模型、客户端凭据、上游凭证和热力图六个视图都有真实数据', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const names = ['总览', '趋势分析', '模型分析', '客户端凭据分析', '上游凭证分析', '用量热力图'];
  for (const name of names) await assertVisible(page.getByRole('tab', { name, exact: true }));

  await assertContains(page.getByRole('tabpanel'), 'OpenAI');
  await assertContains(page.getByRole('tabpanel'), '成功');

  await page.getByRole('tab', { name: '趋势分析', exact: true }).click();
  await assertVisible(page.getByRole('img', { name: '请求量时间趋势图', exact: true }));
  assert.ok(await page.locator('.usage-trend-points button').count() > 0, 'trend must expose real data points');

  await page.getByRole('tab', { name: '模型分析', exact: true }).click();
  await assertContains(page.getByRole('tabpanel'), model);

  await page.getByRole('tab', { name: '客户端凭据分析', exact: true }).click();
  await assertContains(page.getByRole('tabpanel'), 'Browser E2E credential');

  await page.getByRole('tab', { name: '上游凭证分析', exact: true }).click();
  await assertContains(page.getByRole('tabpanel'), 'Browser mock upstream');

  await page.getByRole('tab', { name: '用量热力图', exact: true }).click();
  await assertVisible(page.getByRole('img', { name: '按星期和 UTC 小时显示的请求量热力图', exact: true }));
  await assertCount(page.locator('.usage-heatmap-cell'), 168);
  const heatmapTitles = await page.locator('.usage-heatmap-cell').evaluateAll((cells) => cells.map((cell) => cell.getAttribute('title') ?? ''));
  assert.ok(heatmapTitles.some((title) => !title.includes('，0 次请求')), 'heatmap must contain a non-zero cell');

  await page.getByRole('tab', { name: '总览', exact: true }).click();
});

Then('模型、客户端凭据、上游和状态过滤都作用于真实统计 API', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const seed = runtime.requireSeed();
  const controls = page.locator('.usage-controls');

  await applyUsageFilter(page, async () => controls.getByLabel('模型').fill(model), 'model', model, 51);
  await clearUsageFilters(page);
  await applyUsageFilter(page, async () => controls.getByLabel('凭据主键').fill(seed.clientKeyId), 'key_id', seed.clientKeyId, 51);
  await clearUsageFilters(page);
  await applyUsageFilter(page, async () => { await controls.getByLabel('上游提供商').selectOption(seed.upstreamId); }, 'upstream_account_id', seed.upstreamId, 51);
  await clearUsageFilters(page);
  await applyUsageFilter(page, async () => { await controls.getByLabel('状态').selectOption('success'); }, 'status', 'success', 50);
  await applyUsageFilter(page, async () => { await controls.getByLabel('状态').selectOption('error'); }, 'status', 'error', 1);
  await clearUsageFilters(page);
});

When('浏览器提供严格且权重可区分的请求统计维度 fixture', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const observation: StrictUsageObservation = { requestUrls: [] };
  strictUsageObservations.set(this, observation);
  await page.route('**/internal/v1/usage-analysis?**', async (route) => {
    const requestUrl = new URL(route.request().url());
    observation.requestUrls.push(requestUrl.toString());
    const status = requestUrl.searchParams.get('status');
    const upstream = requestUrl.searchParams.get('upstream_account_id');
    const protocol = requestUrl.searchParams.get('protocol');
    const keyId = requestUrl.searchParams.get('key_id');
    if ((status && !['success', 'error'].includes(status))
      || (upstream && upstream !== 'unassigned' && !uuidPattern.test(upstream))
      || (protocol && !['openai', 'anthropic', 'openai-image', 'generation'].includes(protocol))
      || (keyId && !uuidPattern.test(keyId))) {
      await route.fulfill({
        status: 400,
        contentType: 'application/json',
        body: JSON.stringify({ error: { code: 'invalid_filter', message: 'strict usage filter fixture rejected a non-public filter value' } }),
      });
      return;
    }
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify(strictDimensionUsageFixture(requestUrl.searchParams)),
    });
  });
  await connectOperator(this, 'dark');
  await page.getByRole('tab', { name: '请求统计', exact: true }).click();
  await assertExactText(metric(page, '请求数'), '17');
  await eventually(async () => {
    const labels = await page.locator('.usage-filter-link').allTextContents();
    assert.ok(labels.includes('失败'), `strict usage fixture dimension buttons: ${JSON.stringify(labels)}`);
  });
});

Then('点击失败状态 bucket 使用 error 并仅显示失败结果', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const observation = requireStrictUsageObservation(this);
  const requestsBeforeClick = observation.requestUrls.length;
  const statusPanel = usageDimension(page, '状态');
  const failureBucket = statusPanel.locator('.usage-filter-link').filter({ hasText: '失败' });
  await assertAttribute(failureBucket, 'aria-label', '按 失败 筛选用量');
  await failureBucket.click();
  await assertValue(page.locator('.usage-controls').getByLabel('状态'), 'error');
  const requestUrl = await nextStrictUsageUrl(observation, requestsBeforeClick);
  assert.equal(requestUrl.searchParams.get('status'), 'error');
  assert.notEqual(requestUrl.searchParams.get('status'), 'failure');
  await assertExactText(metric(page, '请求数'), '5');
  await assertCount(statusPanel.locator('tbody tr'), 1);
  await assertContains(statusPanel, '失败');
  await assertNoCount(statusPanel.locator('.usage-filter-link').filter({ hasText: '成功' }));
});

Then('点击未分配上游使用 unassigned 并仅显示无上游结果', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await clearStrictUsageFilters(this, 17);
  await page.getByRole('tab', { name: '上游凭证分析', exact: true }).click();
  const upstreamPanel = usageDimension(page, '上游提供商');
  const observation = requireStrictUsageObservation(this);
  const requestsBeforeClick = observation.requestUrls.length;
  const unassignedBucket = upstreamPanel.locator('.usage-filter-link').filter({ hasText: '未分配上游' });
  await assertAttribute(unassignedBucket, 'aria-label', '按 未分配上游 筛选用量');
  await unassignedBucket.click();
  const requestUrl = await nextStrictUsageUrl(observation, requestsBeforeClick);
  assert.equal(requestUrl.searchParams.get('upstream_account_id'), 'unassigned');
  await assertValue(page.locator('.usage-controls').getByLabel('上游提供商'), 'unassigned');
  await assertCount(upstreamPanel.locator('tbody tr'), 1);
  await assertAttribute(upstreamPanel.locator('tbody tr td').nth(1), 'title', '6');
  await assertContains(upstreamPanel, '未分配上游');
  await assertNotContains(upstreamPanel, 'Browser mock upstream');
});

Then('模型、凭据别名、协议和错误码 bucket 使用精确公开过滤值', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const seed = runtime.requireSeed();
  const observation = requireStrictUsageObservation(this);
  const controls = page.locator('.usage-controls');

  await clearStrictUsageFilters(this, 17);
  await page.getByRole('tab', { name: '模型分析', exact: true }).click();
  let requestsBeforeClick = observation.requestUrls.length;
  await usageDimension(page, '模型').locator('.usage-filter-link').filter({ hasText: model }).click();
  let requestUrl = await nextStrictUsageUrl(observation, requestsBeforeClick);
  assert.equal(requestUrl.searchParams.get('model'), model);
  await assertValue(controls.getByLabel('模型'), model);

  await clearStrictUsageFilters(this, 17);
  await page.getByRole('tab', { name: '客户端凭据分析', exact: true }).click();
  requestsBeforeClick = observation.requestUrls.length;
  const credentialBucket = usageDimension(page, '客户端凭据')
    .locator('.usage-filter-link').filter({ hasText: 'Browser E2E credential' });
  await assertAttribute(credentialBucket, 'aria-label', '按 Browser E2E credential 筛选用量');
  await credentialBucket.click();
  requestUrl = await nextStrictUsageUrl(observation, requestsBeforeClick);
  assert.equal(requestUrl.searchParams.get('key_id'), seed.clientKeyId);
  assert.match(requestUrl.searchParams.get('key_id') ?? '', uuidPattern);
  await assertValue(controls.getByLabel('凭据主键'), seed.clientKeyId);

  await clearStrictUsageFilters(this, 17);
  await page.getByRole('tab', { name: '总览', exact: true }).click();
  requestsBeforeClick = observation.requestUrls.length;
  await usageDimension(page, '协议').locator('.usage-filter-link').filter({ hasText: 'OpenAI' }).click();
  requestUrl = await nextStrictUsageUrl(observation, requestsBeforeClick);
  assert.equal(requestUrl.searchParams.get('protocol'), 'openai');
  await assertValue(controls.getByLabel('协议'), 'openai');

  await clearStrictUsageFilters(this, 17);
  requestsBeforeClick = observation.requestUrls.length;
  const errorBucket = usageDimension(page, '错误码')
    .locator('.usage-filter-link').filter({ hasText: 'strict_fixture_error' });
  await errorBucket.click();
  requestUrl = await nextStrictUsageUrl(observation, requestsBeforeClick);
  assert.equal(requestUrl.searchParams.get('error_code'), 'strict_fixture_error');
  await assertValue(controls.getByLabel('错误码'), 'strict_fixture_error');
  await assertExactText(metric(page, '请求数'), '5');
  await assertCount(usageDimension(page, '错误码').locator('tbody tr'), 1);
});

Then('真实上游 UUID 和清除过滤保持可用且中英文亮暗主题无回归', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const seed = runtime.requireSeed();
  await clearStrictUsageFilters(this, 17);
  await page.getByRole('tab', { name: '上游凭证分析', exact: true }).click();
  let upstreamPanel = usageDimension(page, '上游提供商');
  const observation = requireStrictUsageObservation(this);
  const requestsBeforeClick = observation.requestUrls.length;
  const assignedBucket = upstreamPanel.locator('.usage-filter-link').filter({ hasText: seed.upstreamName });
  await assertAttribute(assignedBucket, 'aria-label', `按 ${seed.upstreamName} 筛选用量`);
  await assignedBucket.click();
  const assignedUrl = await nextStrictUsageUrl(observation, requestsBeforeClick);
  assert.equal(assignedUrl.searchParams.get('upstream_account_id'), seed.upstreamId);
  assert.match(assignedUrl.searchParams.get('upstream_account_id') ?? '', uuidPattern);
  await assertValue(page.locator('.usage-controls').getByLabel('上游提供商'), seed.upstreamId);
  await assertCount(upstreamPanel.locator('tbody tr'), 1);
  await assertAttribute(upstreamPanel.locator('tbody tr td').nth(1), 'title', '11');
  await assertContains(upstreamPanel, seed.upstreamName);
  await assertNotContains(upstreamPanel, '未分配上游');

  await clearStrictUsageFilters(this, 17);
  upstreamPanel = usageDimension(page, '上游提供商');
  await assertValue(page.locator('.usage-controls').getByLabel('上游提供商'), '');
  await assertCount(upstreamPanel.locator('tbody tr'), 2);
  await assertContains(upstreamPanel, seed.upstreamName);
  await assertContains(upstreamPanel, '未分配上游');

  await page.locator('.rail .language-toggle').click();
  await assertAttribute(page.locator('html'), 'lang', 'en');
  await assertVisible(page.getByRole('tab', { name: 'Upstream credential analysis', exact: true }));
  await assertVisible(page.getByRole('button', { name: 'Filter usage by Unassigned', exact: true }));
  await assertContains(page.locator('.usage-controls').getByLabel('Upstream provider'), 'Unassigned');
  await assertAttribute(page.locator('html'), 'data-theme', 'dark');
  await page.locator('.rail').getByRole('button', { name: 'Switch to light theme' }).click();
  await assertAttribute(page.locator('html'), 'data-theme', 'light');
  await page.locator('.rail').getByRole('button', { name: 'Switch to dark theme' }).click();
  await assertAttribute(page.locator('html'), 'data-theme', 'dark');
  await assertNoHorizontalOverflow(page);
  this.assertNoBrowserFailures();
});

Then('趋势下钻使用 UTC 毫秒完整闭区间', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await page.getByRole('tab', { name: '趋势分析', exact: true }).click();
  const responsePromise = page.waitForResponse((response) => response.url().includes('/internal/v1/usage-analysis?') && response.url().includes('granularity=auto'));
  await page.locator('.usage-trend-points button').first().click();
  const response = await responsePromise;
  assert.equal(response.status(), 200);
  const requestURL = new URL(response.url());
  const from = Number(requestURL.searchParams.get('from_created_at'));
  const to = Number(requestURL.searchParams.get('to_created_at'));
  assert.ok(Number.isSafeInteger(from) && Number.isSafeInteger(to));
  assert.equal(from % 3_600_000, 0, 'hour drilldown must start on a UTC hour boundary');
  assert.equal(to - from, 3_600_000 - 1, 'hour drilldown must include the complete final millisecond');

  const resetResponse = page.waitForResponse((candidate) => candidate.url().includes('/internal/v1/usage-analysis?') && !candidate.url().includes('from_created_at=' + from));
  await page.getByRole('button', { name: '最近 24 小时', exact: true }).click();
  await resetResponse;
  await page.getByRole('tab', { name: '总览', exact: true }).click();
});

Then('中文指标显示 11.12万、亿、USD 与 CNY 并保留精确值', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await page.route('**/internal/v1/usage-analysis?**', async (route) => {
    await route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(localizationUsageFixture()) });
  });
  await page.getByRole('button', { name: '刷新', exact: true }).click();
  await assertExactText(metric(page, '请求数'), '11.12万');
  await assertAttribute(metric(page, '请求数').locator('span'), 'title', '111,227');
  await assertTextMatches(metric(page, '总 Token'), /^1(?:\.\d+)?亿$/);
  await assertAttribute(metric(page, '总 Token').locator('span'), 'title', '100,111,227');
  await assertExactText(metric(page, '输出 Token'), '11.12万');
  await assertAttribute(metric(page, '输出 Token').locator('span'), 'title', '111,227');
  const costs = page.locator('.usage-cost-lines');
  await assertContains(costs, '¥2.5');
  await assertContains(costs, 'US$1.25');
});

When('管理员将请求统计切换为英文', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await page.locator('.rail .language-toggle').click();
  await assertAttribute(page.locator('html'), 'lang', 'en');
  await assertVisible(page.getByRole('tab', { name: 'Request statistics', exact: true }));
  await page.getByRole('tab', { name: 'Overview', exact: true }).click();
});

Then('英文 111,227 使用千分位而非 K 或 M 且亮暗主题均可切换', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await assertExactText(metric(page, 'Requests'), '111,227');
  await assertNotContains(metric(page, 'Requests'), 'K');
  await assertNotContains(metric(page, 'Requests'), 'M');
  await assertExactText(metric(page, 'Total tokens'), '100,111,227');
  await assertExactText(metric(page, 'Input tokens'), '100,000,000');
  await assertExactText(metric(page, 'Output tokens'), '111,227');
  await assertAttribute(page.locator('html'), 'data-theme', 'dark');
  await page.locator('.rail').getByRole('button', { name: 'Switch to light theme' }).click();
  await assertAttribute(page.locator('html'), 'data-theme', 'light');
  await assertAttribute(page.locator('meta[name="theme-color"]'), 'content', '#f4f7f5');
  await page.locator('.rail').getByRole('button', { name: 'Switch to dark theme' }).click();
  await assertAttribute(page.locator('html'), 'data-theme', 'dark');
  await assertAttribute(page.locator('meta[name="theme-color"]'), 'content', '#071014');
});

Then('浏览器没有控制台错误或失败请求', function (this: DogfoodWorld) {
  this.assertNoBrowserFailures();
});

When('请求统计 API 返回空数据', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await page.route('**/internal/v1/usage-analysis?**', async (route) => {
    await route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(emptyUsageFixture()) });
  });
  await page.getByRole('tab', { name: '请求统计', exact: true }).click();
  await assertExactText(metric(page, '请求数'), '0');
});

Then('六个统计视图呈现明确空态', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await assertCount(page.getByText('此维度暂无数据', { exact: true }), 3);
  await page.getByRole('tab', { name: '趋势分析', exact: true }).click();
  await assertVisible(page.getByText('暂无趋势数据', { exact: true }));
  for (const tab of ['模型分析', '客户端凭据分析', '上游凭证分析']) {
    await page.getByRole('tab', { name: tab, exact: true }).click();
    await assertVisible(page.getByText('此维度暂无数据', { exact: true }));
  }
  await page.getByRole('tab', { name: '用量热力图', exact: true }).click();
  await assertVisible(page.getByText('暂无热力图数据', { exact: true }));
});

When('请求统计 API 返回 500', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await page.unroute('**/internal/v1/usage-analysis?**');
  await page.route('**/internal/v1/usage-analysis?**', async (route) => {
    await route.fulfill({
      status: 500,
      contentType: 'application/json',
      body: JSON.stringify({ error: { code: 'synthetic_failure', message: 'synthetic usage failure' } }),
    });
  });
  await page.getByRole('button', { name: '刷新', exact: true }).click();
});

Then('页面呈现安全错误且不保留过期统计', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await assertContains(page.getByRole('alert'), 'synthetic usage failure');
  await assertNoCount(page.locator('.usage-tab-panel'));
  const expectedConsoleError = this.consoleErrors.findIndex((message) =>
    message.includes('Failed to load resource') && message.includes('500 (Internal Server Error)'));
  assert.notEqual(expectedConsoleError, -1, 'the deliberately mocked HTTP 500 must be visible to the browser');
  this.consoleErrors.splice(expectedConsoleError, 1);
  this.assertNoBrowserFailures();
});

When('下游用户以中文亮色主题在手机视口打开自助门户', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const seed = runtime.requireSeed();
  await this.open('/portal', { theme: 'light', locale: 'zh-CN', viewport: { width: 375, height: 812 } });
  await page.locator('input[type="password"]').fill(seed.clientCredential);
  await page.getByRole('button', { name: '载入', exact: true }).click();
});

Then('自助门户显示自己的 51 条请求和分页统计', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await assertExactText(metric(page, '总请求'), '51');
  await assertExactText(metric(page, '成功'), '50');
  await assertExactText(metric(page, '失败'), '1');
  await assertExactText(metric(page, 'Tokens'), '600');
  await assertContains(metric(page, '可用余额 (USD)'), '$');
  await assertContains(metric(page, '总费用'), '$');
  await assertVisible(page.getByRole('heading', { name: 'Browser E2E credential', exact: true }));
  await assertVisible(page.getByText(model, { exact: true }).first());
  await assertCount(page.locator('.self-history tbody tr'), 50);
  await assertVisible(page.getByRole('button', { name: '加载更早请求', exact: true }));

  await page.locator('.mobile-controls .language-toggle').click();
  await assertAttribute(page.locator('html'), 'lang', 'en');
  await assertExactText(metric(page, 'Tokens'), '600');
  await assertContains(metric(page, 'Available balance (USD)'), '$');
  await assertContains(metric(page, 'Total cost'), '$');
  await page.locator('.mobile-controls .language-toggle').click();
  await assertAttribute(page.locator('html'), 'lang', 'zh-CN');
});

Then('自助门户显示自己的余额、速率、并发和预算快照', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const snapshot = page.getByRole('heading', { name: '当前额度与限流状态', exact: true }).locator('..');
  await assertVisible(snapshot);
  await assertContains(snapshot, '可用余额 (USD)');
  await assertContains(snapshot, 'RPM');
  await assertContains(snapshot, 'TPM');
  await assertContains(snapshot, '并发');
  await assertContains(snapshot, '每日额度');
  await assertContains(snapshot, '每周额度');
  await assertContains(snapshot, '总可用额度');
});

When('下游用户筛选失败请求并打开详情', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const seed = runtime.requireSeed();
  const filters = page.locator('.self-request-filters');
  await filters.getByLabel('上游主键').fill(seed.upstreamId);
  await filters.getByLabel('路由主键').fill(seed.routeId);
  await filters.getByLabel('最低费用').fill('0');
  await filters.getByLabel('最高费用').fill('1000');
  await filters.getByRole('button', { name: '应用筛选' }).click();
  await assertExactText(metric(page, '总请求'), '51');
  await assertCount(page.locator('.self-history tbody tr'), 50);
  await page.getByRole('button', { name: '按 http_429 筛选请求' }).click();
  await assertValue(filters.getByLabel('状态'), 'error');
  await assertValue(filters.getByLabel('错误码'), 'http_429');
  await assertCount(page.locator('.self-history tbody tr'), 1);
  await assertContains(page.locator('.self-history'), 'http_429');
  await page.locator('.self-history').getByRole('button', { name: /请求详情$/ }).click();
});

Then('只能看到自己的错误正文且清除筛选后可加载完整历史', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const drawer = page.getByRole('dialog');
  await assertContains(drawer, '429');
  await assertContains(drawer, 'upstream rejected the request');
  await drawer.getByRole('button', { name: '关闭', exact: true }).click();

  const filters = page.locator('.self-request-filters');
  await filters.getByRole('button', { name: '清除筛选', exact: true }).click();
  await assertCount(page.locator('.self-history tbody tr'), 50);
  await page.getByRole('button', { name: '加载更早请求', exact: true }).click();
  await assertCount(page.locator('.self-history tbody tr'), 51);
  await assertNoCount(page.locator('.notice.error'));
  await assertAttribute(page.locator('html'), 'data-theme', 'light');
  await assertNoHorizontalOverflow(page);
});

Then('下游凭据不能读取管理资源或选择另一个凭据身份', async function () {
  const seed = runtime.requireSeed();
  const forbiddenManagementRead = await fetch(new URL('/internal/v1/tenants', baseURL), {
    headers: { Authorization: `Bearer ${seed.clientCredential}` },
  });
  assert.ok([401, 403].includes(forbiddenManagementRead.status));

  const cannotSelectAnotherCredential = await fetch(new URL(`/self/v1/stats?key_id=${seed.otherClientKeyId}`, baseURL), {
    headers: { Authorization: `Bearer ${seed.clientCredential}` },
  });
  assert.equal(cannotSelectAnotherCredential.status, 200);
  const body = await cannotSelectAnotherCredential.json() as { key_id: string };
  assert.equal(body.key_id, seed.clientKeyId);
});

When('下游用户输入无效凭据并切换英文', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await page.locator('input[type="password"]').fill('invalid-browser-test-credential');
  await page.getByRole('button', { name: '载入', exact: true }).click();
  await assertContains(page.getByRole('alert'), '凭据无效或已失效');
  await page.locator('.mobile-controls .language-toggle').click();
  await page.getByRole('button', { name: 'Load', exact: true }).click();
});

Then('中英文都显示安全的无效凭据提示且浏览器没有失败', async function (this: DogfoodWorld) {
  await assertContains(this.requirePage().getByRole('alert'), 'invalid or no longer active');
  await this.requirePage().waitForTimeout(100);
  assert.ok(this.consoleErrors.length > 0, 'invalid credentials must produce unauthorized resource responses');
  assert.ok(
    this.consoleErrors.every((message) =>
      message.includes('Failed to load resource') && message.includes('401 (Unauthorized)')),
    `unexpected console error while checking invalid credentials: ${JSON.stringify(this.consoleErrors)}`,
  );
  this.consoleErrors.splice(0);
  this.assertNoBrowserFailures();
});

When('管理员以中文亮色主题打开模型计费', async function (this: DogfoodWorld) {
  await connectOperator(this, 'light');
  const page = this.requirePage();
  await page.getByRole('tab', { name: '模型计费', exact: true }).click();
  await assertVisible(page.getByRole('heading', { name: '多模态生成价格', exact: true }));
});

Then('多模态模型 {string} 以 {string} 计费并显示价格 {string}', async function (
  this: DogfoodWorld,
  priceModel: string,
  unit: string,
  price: string,
) {
  const page = this.requirePage();
  const pricingPanel = page.locator('article.panel').filter({ has: page.getByRole('heading', { name: '多模态生成价格', exact: true }) });
  const row = pricingPanel.locator('tbody tr').filter({ hasText: priceModel });
  await assertContains(row, unit);
  await assertContains(row, price);
  this.assertNoBrowserFailures();
});

When('浏览器模拟实时请求流断线超过五秒并重放最后事件', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const eventAt = Date.now();
  const baseline = requestEventFixture(
    '019f0000-0000-7000-8000-000000000001',
    '019f0000-0000-7000-9000-000000000001',
    eventAt,
    'browser-sse-baseline',
  );
  const missingOne = requestEventFixture(
    '019f0000-0000-7000-8000-000000000002',
    '019f0000-0000-7000-9000-000000000002',
    eventAt,
    'browser-sse-missing-one',
  );
  const missingTwo = requestEventFixture(
    '019f0000-0000-7000-8000-000000000003',
    '019f0000-0000-7000-9000-000000000003',
    eventAt,
    'browser-sse-missing-two',
  );
  const connectionUrls: string[] = [];
  let tenantConnections = 0;
  let firstClosedAt = 0;
  let secondDeliveredAt = 0;

  await page.route('**/internal/v1/request-events**', async (route) => {
    const requestUrl = new URL(route.request().url());
    if (requestUrl.searchParams.get('tenant_external_id') !== tenant) {
      await route.continue();
      return;
    }
    connectionUrls.push(requestUrl.toString());
    tenantConnections += 1;
    if (tenantConnections === 1) {
      await route.fulfill({ status: 200, contentType: 'text/event-stream', body: sseRequestEvent(baseline) });
      firstClosedAt = Date.now();
      return;
    }
    if (tenantConnections === 2) {
      await new Promise((resolve) => setTimeout(resolve, 6_000));
      await route.fulfill({
        status: 200,
        contentType: 'text/event-stream',
        body: [baseline, missingOne, missingTwo].map(sseRequestEvent).join(''),
      });
      secondDeliveredAt = Date.now();
      return;
    }
    await route.continue();
  });

  await connectOperator(this, 'dark');
  await assertCount(page.locator('#operator-panel-traffic tbody tr').filter({ hasText: baseline.model }), 1);
  await assertCount(page.locator('#operator-panel-traffic tbody tr').filter({ hasText: missingOne.model }), 1);
  await assertCount(page.locator('#operator-panel-traffic tbody tr').filter({ hasText: missingTwo.model }), 1);
  await eventually(() => assert.ok(connectionUrls.length >= 3), 15_000, 'realtime stream did not reconnect after the mocked close');

  realtimeReconnectObservations.set(this, {
    connectionUrls,
    disconnectedForMs: secondDeliveredAt - firstClosedAt,
    finalCursorId: missingTwo.event_id,
    finalRowCount: await page.locator('#operator-panel-traffic tbody tr').count(),
  });
});

Then('控制台使用双游标只补齐缺失请求且正常关闭和切页均不报错', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const observation = realtimeReconnectObservations.get(this);
  assert.ok(observation, 'realtime reconnect observation is missing');
  assert.ok(observation.disconnectedForMs > 5_000, `mocked disconnect lasted only ${observation.disconnectedForMs}ms`);

  const firstReconnect = new URL(observation.connectionUrls[1]);
  assert.ok(Number.isSafeInteger(Number(firstReconnect.searchParams.get('after_event_at'))));
  assert.equal(firstReconnect.searchParams.get('after_event_id'), '019f0000-0000-7000-8000-000000000001');
  const caughtUpReconnect = new URL(observation.connectionUrls[2]);
  assert.equal(caughtUpReconnect.searchParams.get('after_event_id'), observation.finalCursorId);
  assert.ok(Number.isSafeInteger(Number(caughtUpReconnect.searchParams.get('after_event_at'))));

  assert.ok(observation.finalRowCount >= 3, 'the caught-up request table must contain all mocked events');
  await assertCount(page.locator('#operator-panel-traffic tbody tr'), observation.finalRowCount);
  for (const eventModel of ['browser-sse-baseline', 'browser-sse-missing-one', 'browser-sse-missing-two']) {
    await assertCount(page.locator('#operator-panel-traffic tbody tr').filter({ hasText: eventModel }), 1);
  }
  await assertNoCount(page.locator('.notice.error'));

  const connectionsBeforeTabChange = observation.connectionUrls.length;
  await page.getByRole('tab', { name: '请求统计', exact: true }).click();
  await page.getByRole('tab', { name: '实时请求', exact: true }).click();
  await eventually(
    () => assert.ok(observation.connectionUrls.length > connectionsBeforeTabChange),
    10_000,
    'realtime stream did not reconnect after returning to the traffic tab',
  );
  const afterAbort = new URL(observation.connectionUrls.at(-1)!);
  assert.equal(afterAbort.searchParams.get('after_event_id'), observation.finalCursorId);
  await assertNoCount(page.locator('.notice.error'));
  await assertCount(page.locator('#operator-panel-traffic tbody tr'), observation.finalRowCount);

  const filters = page.locator('.traffic-filters');
  await filters.getByLabel('模型').fill(model);
  await filters.getByRole('button', { name: '应用筛选', exact: true }).click();
  await assertVisible(page.getByRole('heading', { name: '筛选请求', exact: true }));
  const connectionsBeforeFilterReset = observation.connectionUrls.length;
  await filters.getByRole('button', { name: '清除筛选', exact: true }).click();
  await eventually(
    () => assert.ok(observation.connectionUrls.length > connectionsBeforeFilterReset),
    10_000,
    'realtime stream did not restart after clearing filters',
  );
  const afterFilterReset = new URL(observation.connectionUrls.at(-1)!);
  assert.equal(afterFilterReset.searchParams.has('after_event_at'), false);
  assert.equal(afterFilterReset.searchParams.has('after_event_id'), false);

  const tenantPicker = page.locator('.tenant-picker select');
  await tenantPicker.selectOption('');
  await assertValue(tenantPicker, '');
  const connectionsBeforeTenantReset = observation.connectionUrls.length;
  await tenantPicker.selectOption(tenant);
  await assertValue(tenantPicker, tenant);
  await eventually(
    () => assert.ok(observation.connectionUrls.length > connectionsBeforeTenantReset),
    10_000,
    'realtime stream did not restart after changing tenants',
  );
  const afterTenantReset = new URL(observation.connectionUrls.at(-1)!);
  assert.equal(afterTenantReset.searchParams.has('after_event_at'), false);
  assert.equal(afterTenantReset.searchParams.has('after_event_id'), false);
  await assertNoCount(page.locator('.notice.error'));
  this.assertNoBrowserFailures();
});

async function connectOperator(world: DogfoodWorld, theme: 'dark' | 'light'): Promise<void> {
  const page = world.requirePage();
  const seed = runtime.requireSeed();
  await world.open('/operator', { theme, locale: 'zh-CN' });
  await page.locator('input[type="password"]').fill(seed.serviceCredential);
  await page.getByRole('button', { name: '连接', exact: true }).click();
  const tenantPicker = page.locator('.tenant-picker select');
  await assertContains(tenantPicker, tenant);
  await tenantPicker.selectOption(tenant);
  await assertValue(tenantPicker, tenant);
  await assertNoCount(page.locator('.notice.error'));
}

function requestEventFixture(eventId: string, requestId: string, eventAt: number, eventModel: string) {
  return {
    event_id: eventId,
    request_id: requestId,
    event_at: eventAt,
    event_kind: 'finished' as const,
    key_id: '019f0000-0000-7000-a000-000000000001',
    protocol: 'openai',
    model: eventModel,
    status_code: 200,
    duration_ms: 42,
    input_tokens: 5,
    output_tokens: 7,
    cost: '0.000019',
    error_code: null,
  };
}

function sseRequestEvent(event: ReturnType<typeof requestEventFixture>): string {
  return `id: ${event.event_id}\nevent: request.${event.event_kind}\ndata: ${JSON.stringify(event)}\n\n`;
}

function metric(page: Page, label: string): Locator {
  return page.locator('.metric').filter({ hasText: label }).locator('strong');
}

async function assertVisible(locator: Locator): Promise<void> {
  await locator.first().waitFor({ state: 'visible', timeout: 10_000 });
}

async function assertContains(locator: Locator, expected: string): Promise<void> {
  await eventually(async () => {
    const text = (await locator.first().textContent()) ?? '';
    assert.ok(text.includes(expected), `expected ${JSON.stringify(text)} to contain ${JSON.stringify(expected)}`);
  });
}

async function assertNotContains(locator: Locator, expected: string): Promise<void> {
  await eventually(async () => {
    const text = (await locator.first().textContent()) ?? '';
    assert.ok(!text.includes(expected), `expected ${JSON.stringify(text)} not to contain ${JSON.stringify(expected)}`);
  });
}

async function assertExactText(locator: Locator, expected: string): Promise<void> {
  await eventually(async () => {
    assert.equal(((await locator.first().textContent()) ?? '').trim(), expected);
  });
}

async function assertTextMatches(locator: Locator, expected: RegExp): Promise<void> {
  await eventually(async () => {
    assert.match(((await locator.first().textContent()) ?? '').trim(), expected);
  });
}

async function assertCount(locator: Locator, expected: number): Promise<void> {
  await eventually(async () => assert.equal(await locator.count(), expected));
}

async function assertNoCount(locator: Locator): Promise<void> {
  await assertCount(locator, 0);
}

async function assertValue(locator: Locator, expected: string): Promise<void> {
  await eventually(async () => assert.equal(await locator.inputValue(), expected));
}

async function assertAttribute(locator: Locator, name: string, expected: string): Promise<void> {
  await eventually(async () => assert.equal(await locator.first().getAttribute(name), expected));
}

async function applyUsageFilter(
  page: Page,
  change: () => Promise<void>,
  parameter: string,
  expectedValue: string,
  expectedRequests: number,
): Promise<void> {
  await change();
  const responsePromise = page.waitForResponse((response) => {
    if (!response.url().includes('/internal/v1/usage-analysis?')) return false;
    return new URL(response.url()).searchParams.get(parameter) === expectedValue;
  });
  await page.locator('.usage-controls').getByRole('button', { name: '应用', exact: true }).click();
  const response = await responsePromise;
  assert.equal(response.status(), 200);
  await page.getByRole('tab', { name: '总览', exact: true }).click();
  await assertExactText(metric(page, '请求数'), String(expectedRequests));
}

async function clearUsageFilters(page: Page, expectedRequests = 51): Promise<void> {
  const responsePromise = page.waitForResponse((response) => {
    if (!response.url().includes('/internal/v1/usage-analysis?')) return false;
    const query = new URL(response.url()).searchParams;
    return !query.has('model') && !query.has('key_id') && !query.has('upstream_account_id') && !query.has('status');
  });
  await page.locator('.usage-controls').getByRole('button', { name: '清除筛选', exact: true }).click();
  const response = await responsePromise;
  assert.equal(response.status(), 200);
  await assertExactText(metric(page, '请求数'), String(expectedRequests));
}

function usageDimension(page: Page, heading: string) {
  return page.locator('.usage-dimension').filter({ hasText: heading }).first();
}

function requireStrictUsageObservation(world: DogfoodWorld) {
  const observation = strictUsageObservations.get(world);
  assert.ok(observation, 'strict usage fixture observation is missing');
  return observation;
}

async function nextStrictUsageUrl(observation: StrictUsageObservation, previousCount: number) {
  await eventually(
    () => assert.ok(observation.requestUrls.length > previousCount),
    10_000,
    'strict usage fixture did not receive the dimension drilldown request',
  );
  return new URL(observation.requestUrls.at(-1)!);
}

async function clearStrictUsageFilters(world: DogfoodWorld, expectedRequests: number) {
  const page = world.requirePage();
  const observation = requireStrictUsageObservation(world);
  const previousCount = observation.requestUrls.length;
  await page.locator('.usage-controls').getByRole('button', { name: '清除筛选', exact: true }).click();
  const requestUrl = await nextStrictUsageUrl(observation, previousCount);
  assert.equal(requestUrl.searchParams.has('status'), false);
  assert.equal(requestUrl.searchParams.has('upstream_account_id'), false);
  assert.equal(requestUrl.searchParams.has('model'), false);
  assert.equal(requestUrl.searchParams.has('key_id'), false);
  assert.equal(requestUrl.searchParams.has('protocol'), false);
  assert.equal(requestUrl.searchParams.has('error_code'), false);
  const summaryMetric = metric(page, '请求数');
  if (await summaryMetric.count()) {
    await assertExactText(summaryMetric, String(expectedRequests));
  } else {
    await eventually(async () => {
      const requestCounts = await page.locator('.usage-tab-panel .usage-dimension tbody tr td:nth-child(2)')
        .evaluateAll((cells) => cells.map((cell) => Number(cell.getAttribute('title'))));
      assert.equal(requestCounts.reduce((sum, count) => sum + count, 0), expectedRequests);
    });
  }
}

function usageMetrics(overrides: Record<string, unknown> = {}) {
  return {
    requests: 111_227,
    success: 111_226,
    failed: 1,
    input_tokens: 100_000_000,
    output_tokens: 111_227,
    cached_input_tokens: 0,
    cache_write_tokens: 0,
    generation_units: 0,
    avg_duration_ms: 18.5,
    p95_duration_ms: 25,
    costs: [{ currency: 'CNY', cost: '2.5' }, { currency: 'USD', cost: '1.25' }],
    ...overrides,
  };
}

function localizationUsageFixture() {
  const bucketStart = Date.UTC(2026, 7, 16, 12);
  const summary = usageMetrics();
  return {
    from_created_at: bucketStart,
    to_created_at: bucketStart + 3_600_000 - 1,
    granularity: 'hour',
    time_zone: 'UTC',
    p95_is_approximate: true,
    p95_method: 'fixed_histogram_upper_bound_capped_60000ms',
    summary,
    time_series: [{ bucket_start: bucketStart, ...summary }],
    by_model: [{ id: model, label: model, ...summary }],
    by_key: [{ id: runtime.requireSeed().clientKeyId, label: 'Browser E2E credential', ...summary }],
    by_upstream: [{ id: runtime.requireSeed().upstreamId, label: runtime.requireSeed().upstreamName, ...summary }],
    by_protocol: [{ id: 'openai', label: 'openai', ...summary }],
    by_status: [
      { id: 'success', label: 'success', ...usageMetrics({ requests: 111_226, success: 111_226, failed: 0 }) },
      { id: 'error', label: 'failed', ...usageMetrics({ requests: 1, success: 0, failed: 1 }) },
    ],
    errors: [{ id: 'http_429', label: 'http_429', ...usageMetrics({ requests: 1, success: 0, failed: 1 }) }],
    heatmap: [{ hour_of_week: 12, ...summary }],
  };
}

function strictDimensionUsageFixture(query: URLSearchParams) {
  const seed = runtime.requireSeed();
  const status = query.get('status');
  const upstream = query.get('upstream_account_id');
  const errorCode = query.get('error_code');
  let requests = 17;
  let success = 12;
  let failed = 5;
  let statuses = [
    { id: 'success', label: 'success', ...dimensionUsageMetrics(12, 12, 0) },
    { id: 'error', label: 'failed', ...dimensionUsageMetrics(5, 0, 5) },
  ];
  let upstreams = [
    { id: seed.upstreamId, label: seed.upstreamName, ...dimensionUsageMetrics(11, 8, 3) },
    { id: 'unassigned', label: 'Unassigned', ...dimensionUsageMetrics(6, 4, 2) },
  ];
  if (status === 'error') {
    requests = 5; success = 0; failed = 5;
    statuses = [{ id: 'error', label: 'failed', ...dimensionUsageMetrics(5, 0, 5) }];
    upstreams = [
      { id: seed.upstreamId, label: seed.upstreamName, ...dimensionUsageMetrics(3, 0, 3) },
      { id: 'unassigned', label: 'Unassigned', ...dimensionUsageMetrics(2, 0, 2) },
    ];
  } else if (status === 'success') {
    requests = 12; success = 12; failed = 0;
    statuses = [{ id: 'success', label: 'success', ...dimensionUsageMetrics(12, 12, 0) }];
    upstreams = [
      { id: seed.upstreamId, label: seed.upstreamName, ...dimensionUsageMetrics(8, 8, 0) },
      { id: 'unassigned', label: 'Unassigned', ...dimensionUsageMetrics(4, 4, 0) },
    ];
  } else if (errorCode === 'strict_fixture_error') {
    requests = 5; success = 0; failed = 5;
    statuses = [{ id: 'error', label: 'failed', ...dimensionUsageMetrics(5, 0, 5) }];
    upstreams = [
      { id: seed.upstreamId, label: seed.upstreamName, ...dimensionUsageMetrics(3, 0, 3) },
      { id: 'unassigned', label: 'Unassigned', ...dimensionUsageMetrics(2, 0, 2) },
    ];
  } else if (errorCode) {
    requests = 0; success = 0; failed = 0;
    statuses = [];
    upstreams = [];
  } else if (upstream === 'unassigned') {
    requests = 6; success = 4; failed = 2;
    statuses = [
      { id: 'success', label: 'success', ...dimensionUsageMetrics(4, 4, 0) },
      { id: 'error', label: 'failed', ...dimensionUsageMetrics(2, 0, 2) },
    ];
    upstreams = [{ id: 'unassigned', label: 'Unassigned', ...dimensionUsageMetrics(6, 4, 2) }];
  } else if (upstream) {
    requests = 11; success = 8; failed = 3;
    statuses = [
      { id: 'success', label: 'success', ...dimensionUsageMetrics(8, 8, 0) },
      { id: 'error', label: 'failed', ...dimensionUsageMetrics(3, 0, 3) },
    ];
    upstreams = [{ id: seed.upstreamId, label: seed.upstreamName, ...dimensionUsageMetrics(11, 8, 3) }];
  }
  const summary = dimensionUsageMetrics(requests, success, failed);
  const from = Number(query.get('from_created_at'));
  const to = Number(query.get('to_created_at'));
  const bucketStart = Math.floor((Number.isSafeInteger(from) ? from : Date.now()) / 3_600_000) * 3_600_000;
  return {
    from_created_at: Number.isSafeInteger(from) ? from : bucketStart,
    to_created_at: Number.isSafeInteger(to) ? to : bucketStart + 3_600_000 - 1,
    granularity: 'hour',
    time_zone: 'UTC',
    p95_is_approximate: true,
    p95_method: 'fixed_histogram_upper_bound_capped_60000ms',
    summary,
    time_series: [{ bucket_start: bucketStart, ...summary }],
    by_model: [{ id: model, label: model, ...summary }],
    by_key: [{ id: seed.clientKeyId, label: 'Browser E2E credential', ...summary }],
    by_upstream: upstreams,
    by_protocol: [{ id: 'openai', label: 'openai', ...summary }],
    by_status: statuses,
    errors: failed ? [{ id: 'strict_fixture_error', label: 'strict_fixture_error', ...dimensionUsageMetrics(failed, 0, failed) }] : [],
    heatmap: [{ hour_of_week: 12, ...summary }],
  };
}

function dimensionUsageMetrics(requests: number, success: number, failed: number) {
  return usageMetrics({
    requests,
    success,
    failed,
    input_tokens: requests * 10,
    output_tokens: requests * 2,
    avg_duration_ms: requests ? 20 : null,
    p95_duration_ms: requests ? 40 : null,
    costs: requests ? [{ currency: 'USD', cost: (requests / 100).toFixed(2) }] : [],
  });
}

function emptyUsageFixture() {
  const empty = usageMetrics({
    requests: 0,
    success: 0,
    failed: 0,
    input_tokens: 0,
    output_tokens: 0,
    avg_duration_ms: null,
    p95_duration_ms: null,
    costs: [],
  });
  return {
    from_created_at: Date.UTC(2026, 7, 16),
    to_created_at: Date.UTC(2026, 7, 16, 23, 59, 59, 999),
    granularity: 'hour',
    time_zone: 'UTC',
    p95_is_approximate: true,
    p95_method: 'fixed_histogram_upper_bound_capped_60000ms',
    summary: empty,
    time_series: [],
    by_model: [],
    by_key: [],
    by_upstream: [],
    by_protocol: [],
    by_status: [],
    errors: [],
    heatmap: [],
  };
}

async function assertNoHorizontalOverflow(page: Page): Promise<void> {
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
  assert.ok(layout.document <= layout.viewport, JSON.stringify(layout, null, 2));
}
