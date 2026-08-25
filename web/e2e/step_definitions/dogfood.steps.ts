import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { Given, Then, When } from '@cucumber/cucumber';
import type { Locator, Page } from 'playwright';
import { baseURL, eventually, model, requestJson, runtime, tenant } from '../support/runtime.js';
import type { DogfoodWorld } from '../support/world.js';

import { assertAttribute, assertContains, assertCount, assertExactText, assertNoCount, assertNoHorizontalOverflow, assertNotContains, assertValue, assertVisible, applyUsageFilter, clearStrictUsageFilters, clearUsageFilters, connectOperator, credentialGroupObservations, emptyUsageFixture, groupedModel, localizationUsageFixture, metric, nextStrictUsageUrl, requireStrictUsageObservation, strictDimensionUsageFixture, strictUsageObservations, usageDimension, uuidPattern, type StrictUsageObservation } from './dogfood.support.js';

Given('dogfood 服务已有隔离租户、统一上游、请求记录和多模态价格', function () {
  runtime.requireSeed();
});

When('管理员和下游用户验证凭据记忆与手动清空', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const seed = runtime.requireSeed();
  await connectOperator(this, 'dark');
  await page.reload();
  await assertValue(page.locator('input[type="password"]'), seed.serviceCredential);
  await assertContains(page.locator('.tenant-picker'), tenant);
  await page.getByRole('button', { name: '清空凭据', exact: true }).click();
  await assertValue(page.locator('input[type="password"]'), '');
  await page.reload();
  await assertValue(page.locator('input[type="password"]'), '');

  await this.open('/portal', { theme: 'light', locale: 'zh-CN', viewport: { width: 375, height: 812 } });
  const credential = page.getByLabel('客户端凭据', { exact: true });
  await credential.fill(seed.clientCredential);
  await page.getByRole('button', { name: '载入', exact: true }).click();
  await assertContains(page.locator('.console-context'), 'Browser E2E credential');
  await page.reload();
  await assertValue(credential, seed.clientCredential);
  await assertContains(page.locator('.console-context'), 'Browser E2E credential');
  await page.getByRole('button', { name: '清空凭据', exact: true }).click();
  await assertValue(credential, '');
  await page.reload();
  await assertValue(page.getByLabel('客户端凭据', { exact: true }), '');
  assert.deepEqual(await page.evaluate(() => ({
    operator: localStorage.getItem('mtc.operator.service-credential.v1'),
    self: localStorage.getItem('mtc.self.client-credential.v1'),
  })), { operator: null, self: null });
});

Then('刷新页面不会要求重复输入且清空后不会自动恢复', function (this: DogfoodWorld) {
  this.assertNoBrowserFailures();
});

When('管理员以中文暗色主题连接控制台', async function (this: DogfoodWorld) {
  await connectOperator(this, 'dark');
});

Then('下游凭据表单使用本地化校验且模型计费可见', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await page.getByRole('tab', { name: '凭据管理', exact: true }).click();
  const createPanel = page.locator('details.create-resource').filter({ hasText: '创建客户端凭据' });
  await assertVisible(createPanel.locator('summary'));
  await createPanel.locator('summary').click();
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

Then('插件配置由 Schema 渲染并可保存租户覆盖', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await page.getByRole('tab', { name: '插件', exact: true }).click();
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
  await assertContains(plugin, '写入版本 1');
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

Then('总览、趋势、模型、客户端凭据、会话、上游账户和热力图七个视图都有真实数据', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const names = ['总览', '趋势分析', '模型分析', '客户端凭据分析', '会话分析', '上游账户分析', '用量热力图'];
  for (const name of names) await assertVisible(page.getByRole('tab', { name, exact: true }));

  await assertContains(page.getByRole('tabpanel'), 'OpenAI');
  await assertContains(page.getByRole('tabpanel'), '成功');

  await page.getByRole('tab', { name: '趋势分析', exact: true }).click();
  await assertVisible(page.getByRole('img', { name: '请求数 · 时间趋势图', exact: true }));
  assert.ok(await page.locator('.usage-trend-points button').count() > 0, 'trend must expose real data points');

  await page.getByRole('tab', { name: '模型分析', exact: true }).click();
  await assertContains(page.getByRole('tabpanel'), model);

  await page.getByRole('tab', { name: '客户端凭据分析', exact: true }).click();
  await assertContains(page.getByRole('tabpanel'), 'Browser E2E credential');

  await page.getByRole('tab', { name: '会话分析', exact: true }).click();
  await assertContains(page.getByRole('tabpanel'), '未关联请求');
  await assertContains(page.getByRole('tabpanel'), '按请求量显示前 100 个会话');
  await assertContains(page.getByRole('tabpanel'), '不提供会话 P95');

  await page.getByRole('tab', { name: '上游账户分析', exact: true }).click();
  await assertContains(page.getByRole('tabpanel'), 'Browser mock upstream');
  await assertContains(page.getByRole('tabpanel'), '按稳定上游账户归集');

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
  await page.getByRole('tab', { name: '上游账户分析', exact: true }).click();
  const upstreamPanel = usageDimension(page, '上游账户');
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
  await page.getByRole('tab', { name: '上游账户分析', exact: true }).click();
  let upstreamPanel = usageDimension(page, '上游账户');
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
  upstreamPanel = usageDimension(page, '上游账户');
  await assertValue(page.locator('.usage-controls').getByLabel('上游提供商'), '');
  await assertCount(upstreamPanel.locator('tbody tr'), 2);
  await assertContains(upstreamPanel, seed.upstreamName);
  await assertContains(upstreamPanel, '未分配上游');

  await page.locator('.rail .language-toggle').click();
  await assertAttribute(page.locator('html'), 'lang', 'en');
  await assertVisible(page.getByRole('tab', { name: 'Upstream account analysis', exact: true }));
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

Then('中文指标显示万、亿、万亿、USD 与 CNY 并保留精确值', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await page.route('**/internal/v1/usage-analysis?**', async (route) => {
    await route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(localizationUsageFixture()) });
  });
  await page.getByRole('button', { name: '刷新', exact: true }).click();
  await assertExactText(metric(page, '请求数'), '11.12万');
  await assertAttribute(metric(page, '请求数').locator('span'), 'title', '111,227');
  await assertExactText(metric(page, '输入 Token'), '1亿');
  await assertAttribute(metric(page, '输入 Token').locator('span'), 'title', '100,000,000');
  await assertExactText(metric(page, '输出 Token'), '11.12万');
  await assertAttribute(metric(page, '输出 Token').locator('span'), 'title', '111,227');
  await assertExactText(metric(page, '缓存写入 Token'), '1万亿');
  await assertAttribute(metric(page, '缓存写入 Token').locator('span'), 'title', '1,000,000,000,000');
  await assertExactText(metric(page, '总 Token'), '1万亿');
  await assertAttribute(metric(page, '总 Token').locator('span'), 'title', '1,000,100,111,227');
  await assertExactText(metric(page, '生成计费单位'), '1.23万');
  await assertAttribute(metric(page, '生成计费单位').locator('span'), 'title', '12,345');
  const protocolGenerationUnits = page.locator('.usage-dimension').filter({ has: page.getByRole('heading', { name: '协议', exact: true }) }).locator('tbody td').nth(3);
  await assertExactText(protocolGenerationUnits, '1.23万');
  await assertAttribute(protocolGenerationUnits, 'title', '12,345');
  const costs = page.locator('.usage-cost-lines');
  await assertContains(costs, '¥2.5');
  await assertContains(costs, 'US$1.25');

  await page.getByRole('tab', { name: '趋势分析', exact: true }).click();
  const trendMetric = page.getByLabel('趋势指标', { exact: true });
  await trendMetric.selectOption('tokens');
  await assertVisible(page.getByRole('img', { name: '总 Token · 时间趋势图', exact: true }));
  await assertContains(page.locator('.usage-trend-points'), '1万亿');
  await assertAttribute(page.locator('.usage-trend-points button span').first(), 'title', '1,000,100,111,227');
  await trendMetric.selectOption('generation_units');
  await assertVisible(page.getByRole('img', { name: '生成计费单位 · 时间趋势图', exact: true }));
  await assertContains(page.locator('.usage-trend-points'), '1.23万');
  await assertAttribute(page.locator('.usage-trend-points button span').first(), 'title', '12,345');

  await trendMetric.selectOption('cost');
  const trendCurrency = page.getByLabel('成本币种', { exact: true });
  await assertValue(trendCurrency, 'CNY');
  await assertVisible(page.getByRole('img', { name: '成本 (CNY) · 时间趋势图', exact: true }));
  await assertContains(page.locator('.usage-trend-points'), '¥2.5');
  await assertNotContains(page.locator('.usage-trend-points'), 'US$1.25');
  await trendCurrency.selectOption('USD');
  await assertVisible(page.getByRole('img', { name: '成本 (USD) · 时间趋势图', exact: true }));
  await assertContains(page.locator('.usage-trend-points'), 'US$1.25');
  await assertNotContains(page.locator('.usage-trend-points'), '¥2.5');

  await trendMetric.selectOption('avg_latency');
  await assertVisible(page.getByRole('img', { name: '平均延迟 · 时间趋势图', exact: true }));
  await assertContains(page.locator('.usage-trend-points'), '18.5 ms');
  await trendMetric.selectOption('p95_latency');
  await assertVisible(page.getByRole('img', { name: 'P95 延迟 · 时间趋势图', exact: true }));
  await assertContains(page.locator('.usage-trend-points'), '25 ms');
  await page.getByRole('tab', { name: '总览', exact: true }).click();
});

When('管理员将请求统计切换为英文', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await page.locator('.rail .language-toggle').click();
  await assertAttribute(page.locator('html'), 'lang', 'en');
  await assertVisible(page.getByRole('tab', { name: 'Request statistics', exact: true }));
  await page.getByRole('tab', { name: 'Overview', exact: true }).click();
});

Then('英文大数使用完整千分位而非 K 或 M 且亮暗主题均可切换', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await assertExactText(metric(page, 'Requests'), '111,227');
  await assertNotContains(metric(page, 'Requests'), 'K');
  await assertNotContains(metric(page, 'Requests'), 'M');
  await assertExactText(metric(page, 'Total tokens'), '1,000,100,111,227');
  await assertExactText(metric(page, 'Input tokens'), '100,000,000');
  await assertExactText(metric(page, 'Output tokens'), '111,227');
  await assertExactText(metric(page, 'Cache-write tokens'), '1,000,000,000,000');
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

Then('七个统计视图呈现明确空态', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await assertCount(page.getByText('此维度暂无数据', { exact: true }), 3);
  await page.getByRole('tab', { name: '趋势分析', exact: true }).click();
  await assertVisible(page.getByText('暂无趋势数据', { exact: true }));
  for (const tab of ['模型分析', '客户端凭据分析', '会话分析', '上游账户分析']) {
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
