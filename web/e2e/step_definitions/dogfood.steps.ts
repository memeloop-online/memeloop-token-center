import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { Given, Then, When } from '@cucumber/cucumber';
import type { Locator, Page } from 'playwright';
import { baseURL, eventually, model, requestJson, runtime, tenant } from '../support/runtime.js';
import type { DogfoodWorld } from '../support/world.js';

import { appPreferenceControls, openAppRoute, openUsageDimension } from './app-route.support.js';
import { assertAttribute, assertContains, assertCount, assertExactText, assertNoCount, assertNoHorizontalOverflow, assertNotContains, assertValue, assertVisible, applyUsageFilter, clearStrictUsageFilters, clearUsageFilters, connectOperator, credentialGroupObservations, emptyUsageFixture, groupedModel, localizationUsageFixture, metric, nextStrictUsageUrl, requireStrictUsageObservation, strictDimensionUsageFixture, strictUsageObservations, usageDimension, uuidPattern, type StrictUsageObservation } from './dogfood.support.js';

Given('dogfood 服务已有隔离租户、统一上游、请求记录和多模态价格', function () {
  runtime.requireSeed();
});

When('管理员和下游用户验证凭据记忆与手动清空', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const seed = runtime.requireSeed();
  await connectOperator(this, 'dark');
  await assertValue(page.locator('.operator-credential input[type="password"]'), '');
  await assertContains(page.locator('.console-context'), '已连接');
  await page.reload();
  await assertValue(page.locator('.operator-credential input[type="password"]'), '');
  await assertContains(page.locator('.console-context'), '已连接');
  await assertContains(page.locator('.tenant-picker'), tenant);

  await this.open('/portal', { theme: 'light', locale: 'zh-CN', viewport: { width: 375, height: 812 } });
  await page.getByLabel('客户端凭据', { exact: true }).fill(seed.clientCredential);
  await page.getByRole('button', { name: '进入', exact: true }).click();
  await assertNoCount(page.getByLabel('客户端凭据', { exact: true }));
  await assertContains(page.locator('.console-context'), 'Browser E2E credential');
  await assertContains(page.locator('.console-context'), '已连接');
  await page.reload();
  await assertNoCount(page.getByLabel('客户端凭据', { exact: true }));
  await assertContains(page.locator('.console-context'), 'Browser E2E credential');
  await page.getByRole('button', { name: '清空凭据', exact: true }).click();
  await assertValue(page.getByLabel('客户端凭据', { exact: true }), '');
  await assertNoCount(page.locator('.console-context'));
  assert.deepEqual(await page.evaluate(() => ({
    operator: localStorage.getItem('mtc.operator.service-credential.v1'),
    self: localStorage.getItem('mtc.self.client-credential.v1'),
  })), { operator: seed.serviceCredential, self: null });
  await page.reload();
  await assertValue(page.getByLabel('客户端凭据', { exact: true }), '');
  await assertNoCount(page.locator('.console-context'));

  await this.open('/operator', { theme: 'dark', locale: 'zh-CN' });
  await assertValue(page.locator('.operator-credential input[type="password"]'), '');
  await assertContains(page.locator('.tenant-picker'), tenant);
  await page.getByRole('button', { name: '清空凭据', exact: true }).click();
  await assertNoCount(page.locator('.console-context'));
  await page.reload();
  await assertValue(page.locator('.operator-credential input[type="password"]'), '');
  await assertNoCount(page.locator('.console-context'));
  assert.deepEqual(await page.evaluate(() => ({
    operator: localStorage.getItem('mtc.operator.service-credential.v1'),
    self: localStorage.getItem('mtc.self.client-credential.v1'),
  })), { operator: null, self: null });
});

Then('凭据不回显但刷新会自动恢复认证且清空后不再恢复', function (this: DogfoodWorld) {
  this.assertNoBrowserFailures();
});

When('管理员以中文暗色主题连接控制台', async function (this: DogfoodWorld) {
  await connectOperator(this, 'dark');
});

Then('下游凭据表单使用本地化校验且模型计费可见', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await openAppRoute(page, 'operator', 'credentials');
  const createPanel = page.locator('details.create-resource').filter({ hasText: '创建客户端凭据' });
  await assertVisible(createPanel.locator('summary'));
  await createPanel.locator('summary').click();
  const createButton = page.getByRole('button', { name: '创建凭据', exact: true });
  await assertVisible(createButton);
  await createButton.click();
  await assertContains(page.locator('.schema-errors'), '请修正');
  await assertNotContains(page.locator('.schema-errors'), 'is required');

  await openAppRoute(page, 'operator', 'pricing');
  await assertVisible(page.getByRole('heading', { name: '模型计费', exact: true }));
  await assertVisible(page.getByText(model, { exact: true }).first());
  await assertContains(page.locator('.pricing-summary'), 'models.dev → LiteLLM → OpenRouter');
});

Then('管理员可以重命名凭据并查看当前限制状态', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const seed = runtime.requireSeed();
  await openAppRoute(page, 'operator', 'credentials');
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
  await openAppRoute(page, 'operator', 'pricing');
});

Then('插件配置由 Schema 渲染并可保存租户覆盖', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await openAppRoute(page, 'operator', 'plugins');
  const plugin = page.locator('.managed-resource').filter({ hasText: 'browser-configuration' });
  await assertContains(plugin, '插件默认值');
  await plugin.getByLabel('Mode').selectOption('configured');
  const saveResponse = page.waitForResponse((response) =>
    response.url().endsWith('/internal/v1/plugins/browser-configuration/configuration')
      && response.request().method() === 'PUT');
  await plugin.getByRole('button', { name: '保存', exact: true }).click();
  assert.equal((await saveResponse).status(), 200);
  await assertContains(page.getByRole('status'), '已保存 browser-configuration 的配置');
  await assertContains(plugin, '租户配置');
});

When('管理员切换为亮色英文界面和手机视口', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await appPreferenceControls(page).getByRole('button', { name: '切换到亮色主题' }).click();
  await assertAttribute(page.locator('html'), 'data-theme', 'light');
  await assertAttribute(page.locator('meta[name="theme-color"]'), 'content', '#f4f7f5');
  await appPreferenceControls(page).getByRole('button', { name: 'English', exact: true }).click();
  await assertAttribute(page.locator('html'), 'lang', 'en');
  await page.setViewportSize({ width: 375, height: 812 });
});

Then('英文导航、主题色和响应式布局均正确且浏览器没有失败', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await openAppRoute(page, 'operator', 'pricing');
  await assertVisible(page.getByRole('heading', { name: 'Model pricing', exact: true }));
  await assertNoHorizontalOverflow(page);
  this.assertNoBrowserFailures();
});

When('管理员进入请求统计', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await openAppRoute(page, 'operator', 'usage');
  await assertVisible(page.getByRole('heading', { name: '用量分析', exact: true }));
  await assertExactText(metric(page, '请求数'), '51');
});

Then('总览、趋势、模型、客户端凭据、会话、上游账户和热力图七个视图都有真实数据', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const names = ['总览', '趋势分析', '维度分析', '用量热力图'];
  for (const name of names) await assertVisible(page.getByRole('tab', { name, exact: true }));

  await assertContains(page.getByRole('tabpanel'), 'OpenAI');
  await assertContains(page.getByRole('tabpanel'), '成功');

  await page.getByRole('tab', { name: '趋势分析', exact: true }).click();
  const throughputChart = page.locator('.usage-echart').filter({ has: page.locator('canvas') }).first();
  await assertVisible(throughputChart);
  await assertAttribute(throughputChart, 'aria-label', '请求数: 成功, 失败');
  assert.ok(await page.locator('.usage-chart-table tbody tr').count() > 0, 'trend must expose its real data table');

  await openUsageDimension(page, '模型');
  await assertContains(page.getByRole('tabpanel'), model);

  await openUsageDimension(page, '客户端凭据');
  await assertContains(page.getByRole('tabpanel'), 'Browser E2E credential');

  await openUsageDimension(page, '会话');
  await assertContains(page.getByRole('tabpanel'), 'Browser E2E credential');
  assert.ok(await usageDimension(page, '会话').locator('tbody tr').count() > 0, 'session dimension must contain data rows');

  await openUsageDimension(page, '上游账户');
  await assertContains(page.getByRole('tabpanel'), 'Browser mock upstream');
  assert.ok(await usageDimension(page, '上游账户').locator('tbody tr').count() > 0, 'upstream dimension must contain data rows');

  await page.getByRole('tab', { name: '用量热力图', exact: true }).click();
  const heatmap = page.locator('.usage-echart-heatmap');
  await assertVisible(heatmap);
  await assertAttribute(heatmap, 'aria-label', '按星期和 UTC 小时显示的请求量热力图');
  assert.ok(await heatmap.locator('canvas').evaluate((element) => {
    const canvas = element as HTMLCanvasElement;
    return canvas.width > 0 && canvas.height > 0;
  }), 'heatmap canvas must be rendered');

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
  await openAppRoute(page, 'operator', 'usage');
  await assertExactText(metric(page, '请求数'), '17');
  await openUsageDimension(page, '状态');
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
  await failureBucket.click();
  await assertValue(page.locator('.usage-controls').getByLabel('状态'), 'error');
  const requestUrl = await nextStrictUsageUrl(observation, requestsBeforeClick);
  assert.equal(requestUrl.searchParams.get('status'), 'error');
  assert.notEqual(requestUrl.searchParams.get('status'), 'failure');
  await assertCount(statusPanel.locator('tbody tr'), 1);
  await assertExactText(statusPanel.locator('tbody tr td').nth(1), '5');
  await assertContains(statusPanel, '失败');
  await assertNoCount(statusPanel.locator('.usage-filter-link').filter({ hasText: '成功' }));
});

Then('点击未分配上游使用 unassigned 并仅显示无上游结果', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await clearStrictUsageFilters(this, 17);
  await openUsageDimension(page, '上游账户');
  const upstreamPanel = usageDimension(page, '上游账户');
  const observation = requireStrictUsageObservation(this);
  const requestsBeforeClick = observation.requestUrls.length;
  const unassignedBucket = upstreamPanel.locator('.usage-filter-link').filter({ hasText: '未分配上游' });
  await unassignedBucket.click();
  const requestUrl = await nextStrictUsageUrl(observation, requestsBeforeClick);
  assert.equal(requestUrl.searchParams.get('upstream_account_id'), 'unassigned');
  await assertValue(page.locator('.usage-controls').getByLabel('上游提供商'), 'unassigned');
  await assertCount(upstreamPanel.locator('tbody tr'), 1);
  await assertExactText(upstreamPanel.locator('tbody tr td').nth(1), '6');
  await assertContains(upstreamPanel, '未分配上游');
  await assertNotContains(upstreamPanel, 'Browser mock upstream');
});

Then('模型、凭据别名、协议和错误码 bucket 使用精确公开过滤值', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const seed = runtime.requireSeed();
  const observation = requireStrictUsageObservation(this);
  const controls = page.locator('.usage-controls');

  await clearStrictUsageFilters(this, 17);
  await openUsageDimension(page, '模型');
  let requestsBeforeClick = observation.requestUrls.length;
  await usageDimension(page, '模型').locator('.usage-filter-link').filter({ hasText: model }).click();
  let requestUrl = await nextStrictUsageUrl(observation, requestsBeforeClick);
  assert.equal(requestUrl.searchParams.get('model'), model);
  await assertValue(controls.getByLabel('模型'), model);

  await clearStrictUsageFilters(this, 17);
  await openUsageDimension(page, '客户端凭据');
  requestsBeforeClick = observation.requestUrls.length;
  const credentialBucket = usageDimension(page, '客户端凭据')
    .locator('.usage-filter-link').filter({ hasText: 'Browser E2E credential' });
  await credentialBucket.click();
  requestUrl = await nextStrictUsageUrl(observation, requestsBeforeClick);
  assert.equal(requestUrl.searchParams.get('key_id'), seed.clientKeyId);
  assert.match(requestUrl.searchParams.get('key_id') ?? '', uuidPattern);
  await assertValue(controls.getByLabel('凭据主键'), seed.clientKeyId);

  await clearStrictUsageFilters(this, 17);
  await openUsageDimension(page, '协议');
  requestsBeforeClick = observation.requestUrls.length;
  await usageDimension(page, '协议').locator('.usage-filter-link').filter({ hasText: 'OpenAI' }).click();
  requestUrl = await nextStrictUsageUrl(observation, requestsBeforeClick);
  assert.equal(requestUrl.searchParams.get('protocol'), 'openai');
  await assertValue(controls.getByLabel('协议'), 'openai');

  await clearStrictUsageFilters(this, 17);
  await openUsageDimension(page, '错误码');
  requestsBeforeClick = observation.requestUrls.length;
  const errorBucket = usageDimension(page, '错误码')
    .locator('.usage-filter-link').filter({ hasText: 'strict_fixture_error' });
  await errorBucket.click();
  requestUrl = await nextStrictUsageUrl(observation, requestsBeforeClick);
  assert.equal(requestUrl.searchParams.get('error_code'), 'strict_fixture_error');
  await assertValue(controls.getByLabel('错误码'), 'strict_fixture_error');
  const errorPanel = usageDimension(page, '错误码');
  await assertCount(errorPanel.locator('tbody tr'), 1);
  await assertExactText(errorPanel.locator('tbody tr td').nth(1), '5');
});

Then('真实上游 UUID 和清除过滤保持可用且中英文亮暗主题无回归', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const seed = runtime.requireSeed();
  await clearStrictUsageFilters(this, 17);
  await openUsageDimension(page, '上游账户');
  let upstreamPanel = usageDimension(page, '上游账户');
  const observation = requireStrictUsageObservation(this);
  const requestsBeforeClick = observation.requestUrls.length;
  const assignedBucket = upstreamPanel.locator('.usage-filter-link').filter({ hasText: seed.upstreamName });
  await assignedBucket.click();
  const assignedUrl = await nextStrictUsageUrl(observation, requestsBeforeClick);
  assert.equal(assignedUrl.searchParams.get('upstream_account_id'), seed.upstreamId);
  assert.match(assignedUrl.searchParams.get('upstream_account_id') ?? '', uuidPattern);
  await assertValue(page.locator('.usage-controls').getByLabel('上游提供商'), seed.upstreamId);
  await assertCount(upstreamPanel.locator('tbody tr'), 1);
  await assertExactText(upstreamPanel.locator('tbody tr td').nth(1), '11');
  await assertContains(upstreamPanel, seed.upstreamName);
  await assertNotContains(upstreamPanel, '未分配上游');

  await clearStrictUsageFilters(this, 17);
  await openUsageDimension(page, '上游账户');
  upstreamPanel = usageDimension(page, '上游账户');
  await assertValue(page.locator('.usage-controls').getByLabel('上游提供商'), '');
  await assertCount(upstreamPanel.locator('tbody tr'), 2);
  await assertContains(upstreamPanel, seed.upstreamName);
  await assertContains(upstreamPanel, '未分配上游');

  await appPreferenceControls(page).getByRole('button', { name: 'English', exact: true }).click();
  await assertAttribute(page.locator('html'), 'lang', 'en');
  await openUsageDimension(page, 'Upstream accounts');
  await assertVisible(page.locator('.usage-filter-link').filter({ hasText: 'Unassigned' }));
  await assertContains(page.locator('.usage-controls').getByLabel('Upstream provider'), 'Unassigned');
  await assertAttribute(page.locator('html'), 'data-theme', 'dark');
  await appPreferenceControls(page).getByRole('button', { name: 'Switch to light theme' }).click();
  await assertAttribute(page.locator('html'), 'data-theme', 'light');
  await appPreferenceControls(page).getByRole('button', { name: 'Switch to dark theme' }).click();
  await assertAttribute(page.locator('html'), 'data-theme', 'dark');
  await assertNoHorizontalOverflow(page);
  this.assertNoBrowserFailures();
});

Then('趋势下钻使用 UTC 毫秒完整闭区间', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await page.getByRole('tab', { name: '趋势分析', exact: true }).click();
  const responsePromise = page.waitForResponse((response) => response.url().includes('/internal/v1/usage-analysis?') && response.url().includes('granularity=auto'));
  const chart = page.locator('.usage-chart-card').first().locator('.usage-echart');
  const bounds = await chart.boundingBox();
  assert.ok(bounds, 'throughput chart must have measurable pointer bounds');
  const dataPixel = await chart.locator('canvas').evaluate((element) => {
    const canvas = element as HTMLCanvasElement;
    const context = canvas.getContext('2d');
    if (!context) return undefined;
    const pixels = context.getImageData(0, 0, canvas.width, canvas.height).data;
    const colors = [[104, 222, 201], [255, 156, 114]];
    for (let y = Math.floor(canvas.height * 0.2); y < canvas.height * 0.9; y += 2) {
      for (let x = 0; x < canvas.width; x += 2) {
        const index = (y * canvas.width + x) * 4;
        if (pixels[index + 3] < 180) continue;
        if (colors.some(([red, green, blue]) => Math.abs(pixels[index] - red) < 18
          && Math.abs(pixels[index + 1] - green) < 18 && Math.abs(pixels[index + 2] - blue) < 18)) {
          return { x: x * canvas.clientWidth / canvas.width, y: y * canvas.clientHeight / canvas.height };
        }
      }
    }
    return undefined;
  });
  assert.ok(dataPixel, 'throughput chart must paint at least one data bar');
  await chart.locator('canvas').click({ position: dataPixel });
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

Then('中文指标显示万、亿、万亿、USD 与 CNY 并保留精确值', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await page.route('**/internal/v1/usage-analysis?**', async (route) => {
    await route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(localizationUsageFixture()) });
  });
  await page.getByRole('button', { name: '刷新', exact: true }).click();
  await assertExactText(metric(page, '请求数'), '111,227');
  await assertExactText(metric(page, '总 Token'), '1,000,100,111,227');
  await assertExactText(metric(page, '生成计费单位'), '12,345');
  await assertExactText(metric(page, '缓存 Token'), '0');
  const costs = page.locator('.usage-cost-lines');
  await assertContains(costs, '¥2.5');
  await assertContains(costs, 'US$1.25');

  await page.getByRole('tab', { name: '趋势分析', exact: true }).click();
  await assertCount(page.locator('.usage-chart-card .usage-echart canvas'), 3);
  const trendTable = page.locator('.usage-chart-card').first().locator('.usage-chart-table');
  await trendTable.locator('summary').click();
  await assertContains(trendTable, '1,000,100,111,227');
  await assertContains(trendTable, '18.5 ms');
  await assertContains(trendTable, '25 ms');
  await assertContains(trendTable, '¥2.5');
  await assertContains(trendTable, 'US$1.25');
  await page.getByRole('tab', { name: '总览', exact: true }).click();
});

When('管理员将请求统计切换为英文', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await appPreferenceControls(page).getByRole('button', { name: 'English', exact: true }).click();
  await assertAttribute(page.locator('html'), 'lang', 'en');
  await assertVisible(page.locator('.app-navigation a[href="/operator?view=usage"]'));
  await page.getByRole('tab', { name: 'Overview', exact: true }).click();
});

Then('英文大数使用完整千分位而非 K 或 M 且亮暗主题均可切换', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await assertExactText(metric(page, 'Requests'), '111,227');
  await assertNotContains(metric(page, 'Requests'), 'K');
  await assertNotContains(metric(page, 'Requests'), 'M');
  await assertExactText(metric(page, 'Total tokens'), '1,000,100,111,227');
  await assertExactText(metric(page, 'Generation billing units'), '12,345');
  await assertExactText(metric(page, 'Cached tokens'), '0');
  await assertAttribute(page.locator('html'), 'data-theme', 'dark');
  await appPreferenceControls(page).getByRole('button', { name: 'Switch to light theme' }).click();
  await assertAttribute(page.locator('html'), 'data-theme', 'light');
  await assertAttribute(page.locator('meta[name="theme-color"]'), 'content', '#f4f7f5');
  await appPreferenceControls(page).getByRole('button', { name: 'Switch to dark theme' }).click();
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
  await openAppRoute(page, 'operator', 'usage');
  await assertExactText(metric(page, '请求数'), '0');
});

Then('七个统计视图呈现明确空态', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  for (const dimension of ['模型', '客户端凭据', '会话', '上游账户', '协议', '状态', '错误码']) {
    await openUsageDimension(page, dimension);
    await assertVisible(page.getByText('此维度暂无数据', { exact: true }));
  }
  await page.getByRole('tab', { name: '趋势分析', exact: true }).click();
  await assertCount(page.getByText('当前范围没有可绘制的数据', { exact: true }), 3);
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
