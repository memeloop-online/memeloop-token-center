import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { Given, Then, When } from '@cucumber/cucumber';
import type { Locator, Page } from 'playwright';
import { baseURL, eventually, generationMockCounts, model, requestJson, runtime, tenant } from '../support/runtime.js';
import type { DogfoodWorld } from '../support/world.js';
import { appPreferenceControls, openAppRoute } from './app-route.support.js';
import { assertAttribute, assertContains, assertCount, assertExactText, assertGenerationDownload, assertNoCount, assertNoHorizontalOverflow, assertValue, assertVisible, connectOperator, generationTableFor, metric, multimodalObservations, operatorTrafficPanel, requestEventFixture, realtimeReconnectObservations, requireMultimodalObservation, sseRequestEvent, submitPortalGeneration, uuidPattern, waitForGenerationStatus } from './dogfood.support.js';

const operatorGenerationCancellations = new WeakMap<DogfoodWorld, { status: number; body: { status: string } }>();

When('下游用户以中文亮色主题在手机视口打开自助门户', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const seed = runtime.requireSeed();
  await this.open('/portal', { theme: 'light', locale: 'zh-CN', viewport: { width: 375, height: 812 } });
  await page.locator('input[type="password"]').fill(seed.clientCredential);
  await page.getByRole('button', { name: '进入', exact: true }).click();
});

Then('自助门户显示自己的 51 条请求和分页统计', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await assertExactText(metric(page, '总请求'), '51');
  await assertExactText(metric(page, '成功率'), '98.04%');
  await assertExactText(metric(page, '失败'), '1');
  await assertExactText(metric(page, 'Token 用量'), '600');
  await assertContains(metric(page, '可用余额 (USD)'), '$');
  await assertContains(metric(page, '总费用'), '$');
  await assertVisible(page.getByRole('heading', { name: 'Browser E2E credential', exact: true }));
  await assertVisible(page.getByText(model, { exact: true }).first());
  await assertCount(page.locator('.self-history tbody tr'), 5);

  await appPreferenceControls(page).getByRole('button', { name: 'English', exact: true }).click();
  await assertAttribute(page.locator('html'), 'lang', 'en');
  await assertExactText(metric(page, 'Tokens'), '600');
  await assertContains(metric(page, 'Available balance (USD)'), '$');
  await assertContains(metric(page, 'Total cost'), '$');
  await appPreferenceControls(page).getByRole('button', { name: '中文', exact: true }).click();
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
  await openAppRoute(page, 'portal', 'requests');
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
  await page.getByRole('button', { name: '清空凭据', exact: true }).click();
  await page.locator('input[type="password"]').fill('invalid-browser-test-credential');
  await page.getByRole('button', { name: '进入', exact: true }).click();
  await assertContains(page.getByRole('alert'), '凭据无效或已失效');
  await appPreferenceControls(page).getByRole('button', { name: 'English', exact: true }).click();
  await page.getByRole('button', { name: 'Continue', exact: true }).click();
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
  await connectOperator(this, 'light', runtime.requireSeed().globalServiceCredential);
  const page = this.requirePage();
  await openAppRoute(page, 'operator', 'pricing');
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

When('管理员通过可见表单保存 CNY 多模态价格', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const modelName = 'browser-cny-image-model';
  const manualPricing = page.locator('details.manual-pricing');
  await manualPricing.locator('summary').click();
  await manualPricing.locator('.manual-pricing-body > label select').nth(1).selectOption('CNY');
  await manualPricing.getByLabel('类型').selectOption('generation');
  await manualPricing.getByRole('textbox', { name: '模型', exact: true }).fill(modelName);
  await manualPricing.getByLabel('计费单位').selectOption('image');
  await manualPricing.getByLabel('单位价格').fill('0.88');
  const responsePromise = page.waitForResponse((response) => response.url().includes(`/internal/v1/generation-prices/CNY/${modelName}`) && response.request().method() === 'POST');
  await manualPricing.getByRole('button', { name: '保存手动价格', exact: true }).click();
  assert.equal((await responsePromise).status(), 200);
  await assertValue(page.getByLabel('查看币种', { exact: true }), 'CNY');
});

Then('CNY 价格立即可见且切回 USD 后不会混入', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const pricingPanel = page.locator('article.panel').filter({ has: page.getByRole('heading', { name: '多模态生成价格', exact: true }) });
  const cnyRow = pricingPanel.locator('tbody tr').filter({ hasText: 'browser-cny-image-model' });
  await assertContains(cnyRow, '¥0.88');
  const usdResponse = page.waitForResponse((response) => response.url().includes('/internal/v1/generation-prices?currency=USD'));
  await page.getByLabel('查看币种', { exact: true }).selectOption('USD');
  assert.equal((await usdResponse).status(), 200);
  await assertNoCount(pricingPanel.locator('tbody tr').filter({ hasText: 'browser-cny-image-model' }));
  this.assertNoBrowserFailures();
});

When('管理员通过可见生成任务页查看排队任务详情并取消', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const seed = runtime.requireSeed();
  const jobId = '019f0000-0000-7000-8000-000000000099';
  const job = {
    job_id: jobId,
    created_at: Date.now(),
    updated_at: Date.now(),
    completed_at: null,
    model: 'browser-operator-generation',
    driver: 'comfyui',
    billing_unit: 'job',
    status: 'queued',
    upstream_job_id: null,
    estimated_units: 1,
    billed_units: null,
    cost: '0',
    error_code: null,
    result: null,
    assets: [],
    tenant_external_id: tenant,
    key_id: '019f0000-0000-7000-8000-000000000098',
    key_alias: 'Browser operator job key',
    currency: 'USD',
  };
  await page.route('**/internal/v1/generations?**', async (route) => {
    assert.equal(new URL(route.request().url()).searchParams.get('tenant_external_id'), tenant);
    await route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify([job]) });
  });
  await page.route('**/internal/v1/generations/' + jobId + '?**', async (route) => {
    const url = new URL(route.request().url());
    assert.equal(url.searchParams.get('tenant_external_id'), tenant);
    if (route.request().method() === 'DELETE') {
      const body = { ...job, status: 'cancelled', completed_at: Date.now() };
      operatorGenerationCancellations.set(this, { status: 200, body });
      await route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(body) });
      return;
    }
    assert.equal(route.request().method(), 'GET');
    await route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(job) });
  });
  await connectOperator(this, 'light', seed.globalServiceCredential);
  await openAppRoute(page, 'operator', 'generations');
  const panel = page.locator('.operator-generations');
  await assertContains(panel, 'browser-operator-generation');
  await panel.getByRole('button', { name: '详情', exact: true }).click();
  await assertVisible(page.getByRole('dialog'));
  await assertContains(page.getByRole('dialog'), jobId);
  page.once('dialog', (dialog) => void dialog.accept());
  await page.getByRole('dialog').getByRole('button', { name: '取消', exact: true }).click();
});

Then('生成任务取消请求包含明确租户且页面显示已取消', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const cancellation = operatorGenerationCancellations.get(this);
  assert.equal(cancellation?.status, 200);
  assert.equal(cancellation?.body.status, 'cancelled');
  await assertContains(page.getByRole('status'), '已提交取消请求');
  const panel = page.locator('.operator-generations');
  await assertContains(panel, '已取消');
  this.assertNoBrowserFailures();
});

When('管理员通过真实控件创建多模态上游、价格、路由和凭据', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const seed = runtime.requireSeed();
  const mockBaseUrl = `http://127.0.0.1:${Number(process.env.MTC_E2E_MOCK_PORT ?? 41740)}`;
  const imageModel = 'browser-ui-comfy-image';
  const videoModel = 'browser-ui-seedance-video';

  await connectOperator(this, 'light', seed.globalServiceCredential);
  await openAppRoute(page, 'operator', 'providers');
  const onboarding = page.locator('.provider-onboarding');
  // Successful refresh keeps the workspace and its form state mounted. Open
  // the disclosure only when it is currently collapsed.
  if (!(await onboarding.evaluate((element) => (element as HTMLDetailsElement).open))) {
    await onboarding.locator('summary').click();
  }
  await onboarding.getByLabel('服务提供商').selectOption('comfyui');
  const comfyForm = onboarding.locator('form');
  await comfyForm.locator('#root_name').fill('Browser UI ComfyUI');
  await comfyForm.locator('#root_config_base_url').fill(mockBaseUrl);
  await comfyForm.locator('#root_config_network_scope').selectOption('public');
  await comfyForm.locator('#root_config_workflow_id').fill('browser-workflow-v1');
  const workflowEditor = comfyForm.getByLabel('工作流模板', { exact: true });
  await assertVisible(workflowEditor);
  await workflowEditor.fill(JSON.stringify({
    '5': { class_type: 'EmptyLatentImage', inputs: { width: { $mtc_param: 'width' }, height: { $mtc_param: 'height' }, batch_size: 1 } },
    '9': { class_type: 'SaveImage', inputs: { filename_prefix: { $mtc_param: 'prompt' }, images: ['5', 0] } },
  }, null, 2));
  await comfyForm.getByLabel('下游参数 Schema', { exact: true }).fill(JSON.stringify({
    type: 'object',
    additionalProperties: false,
    required: ['prompt', 'width', 'height'],
    properties: {
      prompt: { type: 'string', minLength: 1, maxLength: 2000 },
      width: { type: 'integer', minimum: 64, maximum: 2048, default: 512 },
      height: { type: 'integer', minimum: 64, maximum: 2048, default: 512 },
    },
  }, null, 2));
  const comfyResponsePromise = page.waitForResponse((response) => response.url().endsWith('/internal/v1/upstreams') && response.request().method() === 'POST');
  await comfyForm.getByRole('button', { name: '添加上游', exact: true }).click();
  const comfyResponse = await comfyResponsePromise;
  assert.equal(comfyResponse.status(), 201, await comfyResponse.text());
  const comfyUpstream = await comfyResponse.json() as { id: string };
  assert.match(comfyUpstream.id, uuidPattern);
  await assertVisible(page.locator('.provider-account').filter({ hasText: 'Browser UI ComfyUI' }));

  if (!(await onboarding.evaluate((element) => (element as HTMLDetailsElement).open))) {
    await onboarding.locator('summary').click();
  }
  await onboarding.getByLabel('提供商').selectOption('volcengine-seedance');
  const providerForm = onboarding.locator('form');
  await providerForm.locator('#root_name').fill('Browser UI Seedance');
  await providerForm.locator('#root_config_base_url').fill(mockBaseUrl);
  const credentialType = providerForm.locator('#root_credential__oneof_select');
  if (await credentialType.count()) await credentialType.selectOption({ index: 1 });
  await providerForm.locator('#root_credential_value').fill('browser-seedance-secret-not-real');
  const upstreamResponsePromise = page.waitForResponse(
    (response) => response.url().endsWith('/internal/v1/upstreams') && response.request().method() === 'POST',
    { timeout: 10_000 },
  ).catch(() => undefined);
  await providerForm.locator('button[type="submit"]').click();
  await page.waitForTimeout(100);
  if (await providerForm.locator('.schema-errors').count()) {
    throw new Error(`Seedance browser form validation failed: ${await providerForm.locator('.schema-errors').innerText()}`);
  }
  const upstreamResponse = await upstreamResponsePromise;
  if (!upstreamResponse) {
    const fields = await providerForm.locator('input, select, textarea').evaluateAll((elements) => elements.map((element) => ({
      id: element.id,
      value: (element as HTMLInputElement).value,
      disabled: (element as HTMLInputElement).disabled,
    })));
    throw new Error(`Seedance browser form did not submit: fields=${JSON.stringify(fields)} console=${JSON.stringify(this.consoleErrors)}`);
  }
  assert.equal(upstreamResponse.status(), 201);
  const seedanceUpstream = await upstreamResponse.json() as { id: string };
  assert.match(seedanceUpstream.id, uuidPattern);
  await assertVisible(page.locator('.provider-account').filter({ hasText: 'Browser UI Seedance' }));

  await openAppRoute(page, 'operator', 'routes');
  const routeForm = page.locator('details.create-resource').filter({ hasText: '创建模型路由' });
  await routeForm.locator('summary').click();
  await routeForm.getByLabel('公开模型').fill(imageModel);
  const imageUpstreamPicker = routeForm.getByRole('combobox', { name: '具体提供商', exact: true });
  await imageUpstreamPicker.fill('Browser UI ComfyUI');
  await routeForm.getByRole('option', { name: /Browser UI ComfyUI/ }).click();
  await assertVisible(routeForm.locator('.selection-chip').filter({ hasText: 'Browser UI ComfyUI' }));
  await routeForm.getByLabel('协议').selectOption('generation');
  await routeForm.getByLabel('上游模型').fill('browser-workflow-v1');
  await routeForm.getByLabel(/未验证的自定义模型/).check();
  const imageRouteResponsePromise = page.waitForResponse((response) => response.url().endsWith('/internal/v1/model-routes') && response.request().method() === 'POST');
  await routeForm.getByRole('button', { name: '创建路由', exact: true }).click();
  const imageRouteResponse = await imageRouteResponsePromise;
  assert.equal(imageRouteResponse.status(), 201, await imageRouteResponse.text());
  const imageRoute = await imageRouteResponse.json() as { id: string };
  assert.match(imageRoute.id, uuidPattern);
  await assertContains(page.getByRole('status'), '路由已创建');

  await routeForm.getByLabel('公开模型').fill(videoModel);
  const upstreamPicker = routeForm.getByRole('combobox', { name: '具体提供商', exact: true });
  await upstreamPicker.fill('Browser UI Seedance');
  await routeForm.getByRole('option', { name: /Browser UI Seedance/ }).click();
  await assertVisible(routeForm.locator('.selection-chip').filter({ hasText: 'Browser UI Seedance' }));
  await routeForm.getByLabel('协议').selectOption('generation');
  const videoCatalogResponsePromise = page.waitForResponse((response) => {
    const url = new URL(response.url());
    return response.request().method() === 'GET'
      && url.pathname === '/internal/v1/upstream-models'
      && url.searchParams.get('account_ids') === seedanceUpstream.id
      && url.searchParams.get('q') === 'seedance-browser-v1';
  }, { timeout: 10_000 });
  await routeForm.getByLabel('上游模型').fill('seedance-browser-v1');
  const videoCatalogResponse = await videoCatalogResponsePromise;
  assert.equal(videoCatalogResponse.status(), 200, await videoCatalogResponse.text());
  const customModelConfirmation = routeForm.getByLabel(/未验证的自定义模型/);
  const createVideoRouteButton = routeForm.getByRole('button', { name: '创建路由', exact: true });
  await eventually(async () => {
    assert.ok(await customModelConfirmation.isVisible() || await createVideoRouteButton.isEnabled(),
      'the video model must be catalogued or offer explicit custom-model confirmation');
  }, 10_000);
  if (await customModelConfirmation.isVisible()) await customModelConfirmation.check();
  await eventually(async () => assert.equal(await createVideoRouteButton.isEnabled(), true), 10_000,
    'the video route did not become valid after selecting its upstream model');
  const routeResponsePromise = page.waitForResponse((response) => response.url().endsWith('/internal/v1/model-routes') && response.request().method() === 'POST');
  await createVideoRouteButton.click();
  const routeResponse = await routeResponsePromise;
  assert.equal(routeResponse.status(), 201);
  const videoRoute = await routeResponse.json() as { id: string };
  assert.match(videoRoute.id, uuidPattern);
  await assertContains(page.getByRole('status'), '路由已创建');

  await openAppRoute(page, 'operator', 'pricing');
  const manualPricing = page.locator('details.manual-pricing');
  await manualPricing.locator('summary').click();
  await manualPricing.getByLabel('类型').selectOption('generation');
  await manualPricing.getByRole('textbox', { name: '模型', exact: true }).fill(imageModel);
  await manualPricing.getByLabel('计费单位').selectOption('job');
  await manualPricing.getByLabel('单位价格').fill('0.2');
  const imagePriceResponsePromise = page.waitForResponse((response) => response.url().includes(`/internal/v1/generation-prices/USD/${imageModel}`) && response.request().method() === 'POST');
  await manualPricing.getByRole('button', { name: '保存手动价格', exact: true }).click();
  assert.equal((await imagePriceResponsePromise).status(), 200);
  await manualPricing.getByRole('textbox', { name: '模型', exact: true }).fill(videoModel);
  await manualPricing.locator('.manual-pricing-body > label select').nth(1).selectOption('USD');
  await manualPricing.getByLabel('计费单位').selectOption('second');
  await manualPricing.getByLabel('单位价格').fill('0.1');
  const priceResponsePromise = page.waitForResponse((response) => response.url().includes(`/internal/v1/generation-prices/USD/${videoModel}`) && response.request().method() === 'POST');
  await manualPricing.getByRole('button', { name: '保存手动价格', exact: true }).click();
  assert.equal((await priceResponsePromise).status(), 200);
  await assertContains(page.getByRole('status'), '价格已保存');

  await openAppRoute(page, 'operator', 'credentials');
  const credentialPanel = page.locator('details.create-resource').filter({ hasText: '创建客户端凭据' });
  await credentialPanel.locator('summary').click();
  const credentialForm = credentialPanel.locator('form');
  await credentialForm.locator('#root_principal_external_id').fill('browser-multimodal-user');
  await credentialForm.locator('#root_alias').fill('Browser multimodal credential');
  await credentialForm.locator('#root_currency').selectOption('USD');
  await credentialForm.locator('#root_initial_balance').fill('10');
  await assertNoCount(credentialForm.locator('#root_policy_allowed_models'));
  const newCredentialRoutes = credentialPanel.getByRole('combobox', { name: '具体路由', exact: true });
  await newCredentialRoutes.fill(imageModel);
  await newCredentialRoutes.press('Enter');
  await newCredentialRoutes.fill(videoModel);
  await newCredentialRoutes.press('Enter');
  const keyResponsePromise = page.waitForResponse((response) => response.url().endsWith('/internal/v1/keys') && response.request().method() === 'POST');
  await credentialForm.getByRole('button', { name: '创建凭据', exact: true }).click();
  const keyResponse = await keyResponsePromise;
  assert.equal(keyResponse.status(), 201);
  const created = await keyResponse.json() as { key: string; key_id: string };
  assert.match(created.key_id, uuidPattern);
  const oneTimeSecret = page.locator('.one-time code');
  await assertExactText(oneTimeSecret, created.key);
  const blocker = await requestJson<{ key: string; key_id: string }>('/internal/v1/keys', {
    method: 'POST', credential: seed.globalServiceCredential,
    body: {
      tenant_external_id: tenant,
      principal_external_id: 'browser-multimodal-worker-blocker',
      alias: 'Browser worker blocker fixture',
      currency: 'USD',
      initial_balance: '1',
      policy: {},
      route_ids: [imageRoute.id],
      route_group_ids: [],
    },
  });
  multimodalObservations.set(this, {
    blockerCredential: blocker.key,
    clientCredential: created.key,
    clientKeyId: created.key_id,
    imageModel,
    videoModel,
    generationResponses: [],
  });
  this.assertNoBrowserFailures();
});

When('普通凭据用户通过中文亮色门户创建图片和两种视频任务', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const observation = requireMultimodalObservation(this);
  page.on('response', (response) => {
    const url = new URL(response.url());
    if (url.origin !== new URL(baseURL).origin) return;
    if (!url.pathname.startsWith('/self/v1/generations')
      && url.pathname !== '/v1/images/generations'
      && url.pathname !== '/v1/videos/generations') return;
    void response.text().then(
      (body) => observation.generationResponses.push(body),
      () => observation.generationResponses.push('__response_body_read_failed__'),
    );
  });
  await this.open('/portal', { theme: 'light', locale: 'zh-CN', viewport: { width: 390, height: 844 } });
  await page.locator('input[type="password"]').fill(observation.clientCredential);
  await page.getByRole('button', { name: '进入', exact: true }).click();
  await openAppRoute(page, 'portal', 'generate');
  await assertVisible(page.getByRole('heading', { name: '创建多模态任务', exact: true }));
  const imageRequest = await submitPortalGeneration(page, 'image', observation.imageModel, '画一个明亮的橙色圆形', '5', { 宽度: '512', 高度: '512' });
  assert.deepEqual(imageRequest, {
    model: observation.imageModel,
    input: { parameters: { width: 512, height: 512, prompt: '画一个明亮的橙色圆形' } },
  });
  await openAppRoute(page, 'portal', 'generations');
  await waitForGenerationStatus(page, observation.imageModel, '已成功');
  await eventually(async () => {
    assert.deepEqual(await generationMockCounts(), { image: 1, video: 0 });
  }, 10_000, 'mock upstream did not receive exactly one image generation');
  await openAppRoute(page, 'portal', 'generate');
  const seedanceRequest = await submitPortalGeneration(page, 'video', observation.videoModel, '一只狐狸跑过草地', '5');
  assert.deepEqual(seedanceRequest, {
    model: observation.videoModel,
    input: { duration: 5, content: [{ type: 'text', text: '一只狐狸跑过草地' }] },
  });
  const comfyVideoRequest = await submitPortalGeneration(
    page, 'video', observation.imageModel, '一只机械鸟掠过城市', '60', { 宽度: '640', 高度: '360' }, false,
  );
  assert.deepEqual(comfyVideoRequest, {
    model: observation.imageModel,
    input: { parameters: { width: 640, height: 360, prompt: '一只机械鸟掠过城市' } },
  });
  await openAppRoute(page, 'portal', 'generations');
});

Then('门户自动轮询到图片和视频成功并显示准确计费', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const observation = requireMultimodalObservation(this);
  const imageRow = await waitForGenerationStatus(page, observation.imageModel, '已成功');
  const videoRow = await waitForGenerationStatus(page, observation.videoModel, '已成功');
  await assertContains(imageRow, '$0.2');
  await assertContains(imageRow, '1');
  await assertContains(videoRow, '$0.5');
  await assertContains(videoRow, '5');
  assert.deepEqual(await generationMockCounts(), { image: 2, video: 1 });
  await assertAttribute(page.locator('html'), 'data-theme', 'light');
});

Then('上游短期签名不出现在响应、页面、详情或持久化存储中', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const observation = requireMultimodalObservation(this);
  const canary = 'never-persist';
  await eventually(() => assert.ok(observation.generationResponses.length >= 4, 'expected browser generation submission and polling responses'));
  const responseBodies = [...observation.generationResponses];
  assert.ok(!responseBodies.includes('__response_body_read_failed__'), 'a generation response body could not be inspected');
  assert.ok(responseBodies.every((body) => !body.includes(canary)), 'provider canary leaked through a browser generation response');
  assert.ok(!(await page.content()).includes(canary), 'provider canary leaked into the rendered portal DOM');

  const jobs = await requestJson<Array<{ job_id: string; model: string }>>('/self/v1/generations?limit=100', {
    credential: observation.clientCredential,
  });
  const video = jobs.find((job) => job.model === observation.videoModel);
  assert.ok(video, 'successful browser video generation was missing from self-service history');
  const detail = await requestJson<unknown>(`/self/v1/generations/${video.job_id}`, {
    credential: observation.clientCredential,
  });
  assert.ok(!JSON.stringify(detail).includes(canary), 'provider canary leaked through self-service generation detail');

  const mockPort = Number(process.env.MTC_E2E_MOCK_PORT ?? 41740);
  const persistenceResponse = await fetch(`http://127.0.0.1:${mockPort}/__e2e/never-persist-state`, {
    signal: AbortSignal.timeout(1_000),
  });
  assert.equal(persistenceResponse.status, 200);
  assert.deepEqual(await persistenceResponse.json(), { database: false, archive: false });
});

Then('用户通过真实下载控件取得归档图片和视频', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const observation = requireMultimodalObservation(this);
  await assertGenerationDownload(page, observation.imageModel, 'browser-result.png', 'browser-png-asset');
  await assertGenerationDownload(page, observation.videoModel, 'asset-0.mp4', 'browser-video-asset');
  this.assertNoBrowserFailures();
});

When('用户通过门户创建并取消排队中的图片任务', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const observation = requireMultimodalObservation(this);
  const blocker = await requestJson<{ job_id: string }>('/v1/images/generations', {
    method: 'POST', credential: observation.blockerCredential,
    body: {
      model: observation.imageModel,
      input: { parameters: { prompt: 'browser-worker-blocker', width: 512, height: 512 } },
    },
  });
  await eventually(async () => {
    assert.ok(blocker.job_id);
    const mockPort = Number(process.env.MTC_E2E_MOCK_PORT ?? 41740);
    const response = await fetch(`http://127.0.0.1:${mockPort}/__e2e/blocker-active`, { signal: AbortSignal.timeout(1_000) });
    assert.equal(response.status, 200);
    assert.equal((await response.json() as { active: boolean }).active, true);
  }, 10_000, 'the deterministic fixture did not occupy the single generation worker');
  await openAppRoute(page, 'portal', 'generate');
  await submitPortalGeneration(page, 'image', observation.imageModel, '这个任务将在排队时取消', '5', { 宽度: '512', 高度: '512' });
  await openAppRoute(page, 'portal', 'generations');
  const generationTable = generationTableFor(page);
  const row = generationTable.locator('tbody tr').filter({ hasText: observation.imageModel }).first();
  const cancellationResponse = page.waitForResponse((response) => response.url().includes('/self/v1/generations/') && response.request().method() === 'DELETE');
  page.once('dialog', (dialog) => void dialog.accept());
  await row.getByRole('button', { name: '取消任务', exact: true }).click();
  const response = await cancellationResponse;
  assert.equal(response.status(), 200, '任务应在 worker 获取 lease 之前由真实门户控件取消');
  await assertContains(row, '已取消');
  await assertContains(row, '$0');
});

Then('取消任务不扣费且请求统计反映多模态用量', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await openAppRoute(page, 'portal', 'overview');
  await assertContains(metric(page, '可用余额 (USD)'), '$9.1');
  await assertContains(metric(page, '总费用'), '$0.9');
  await assertExactText(metric(page, '总请求'), '4');
});

Then('普通凭据在英文暗色主题下仍无法访问管理端', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const observation = requireMultimodalObservation(this);
  await appPreferenceControls(page).getByRole('button', { name: 'English', exact: true }).click();
  await appPreferenceControls(page).getByRole('button', { name: 'Switch to dark theme' }).click();
  await assertAttribute(page.locator('html'), 'lang', 'en');
  await assertAttribute(page.locator('html'), 'data-theme', 'dark');
  await eventually(async () => {
    assert.deepEqual(await page.evaluate(() => ({
      locale: localStorage.getItem('mtc-locale'),
      theme: localStorage.getItem('mtc-theme'),
    })), { locale: 'en', theme: 'dark' });
  });
  assert.ok(this.context);
  const operatorPage = await this.context.newPage();
  operatorPage.on('console', (message) => { if (message.type() === 'error') this.consoleErrors.push(message.text()); });
  operatorPage.on('pageerror', (reason) => this.consoleErrors.push(reason.message));
  await operatorPage.goto('/operator');
  await page.close();
  this.page = operatorPage;
  await assertAttribute(operatorPage.locator('html'), 'lang', 'en');
  await assertAttribute(operatorPage.locator('html'), 'data-theme', 'dark');
  const clearRememberedOperatorCredential = operatorPage.getByRole('button', { name: 'Clear credential', exact: true });
  await assertVisible(clearRememberedOperatorCredential);
  await clearRememberedOperatorCredential.click();
  await assertNoCount(operatorPage.locator('.tenant-picker'));
  await operatorPage.locator('input[type="password"]').fill(observation.clientCredential);
  await operatorPage.getByRole('button', { name: 'Connect', exact: true }).click();
  await eventually(async () => {
    const messages = (await operatorPage.getByRole('alert').allTextContents()).join(' ');
    assert.match(messages, /unauthorized|authentication required|HTTP 401|invalid|credential|permission denied/i);
  });
  await assertNoCount(operatorPage.locator('.tenant-picker'));
  await operatorPage.waitForTimeout(100);
  assert.ok(this.consoleErrors.length > 0, 'the browser must observe rejected management resource requests');
  assert.ok(this.consoleErrors.every((message) => message.includes('401 (Unauthorized)')),
    `unexpected console error while checking management isolation: ${JSON.stringify(this.consoleErrors)}`);
  this.consoleErrors.splice(0);
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
    // Keep this reconnect contract isolated from real fixture events that may finish
    // asynchronously in an earlier scenario. An empty successful tail preserves the
    // last mocked cursor while still exercising close/reconnect and tab resets.
    await route.fulfill({ status: 200, contentType: 'text/event-stream', body: '' });
  });

  await connectOperator(this, 'dark');
  await openAppRoute(page, 'operator', 'requests');
  await assertCount(operatorTrafficPanel(page).locator('tbody tr').filter({ hasText: baseline.model }), 1);
  await assertCount(operatorTrafficPanel(page).locator('tbody tr').filter({ hasText: missingOne.model }), 1);
  await assertCount(operatorTrafficPanel(page).locator('tbody tr').filter({ hasText: missingTwo.model }), 1);
  await eventually(() => assert.ok(connectionUrls.length >= 3), 15_000, 'realtime stream did not reconnect after the mocked close');

  realtimeReconnectObservations.set(this, {
    connectionUrls,
    disconnectedForMs: secondDeliveredAt - firstClosedAt,
    finalCursorId: missingTwo.event_id,
    finalRowCount: await operatorTrafficPanel(page).locator('tbody tr').count(),
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
  await assertCount(operatorTrafficPanel(page).locator('tbody tr'), observation.finalRowCount);
  for (const eventModel of ['browser-sse-baseline', 'browser-sse-missing-one', 'browser-sse-missing-two']) {
    await assertCount(operatorTrafficPanel(page).locator('tbody tr').filter({ hasText: eventModel }), 1);
  }
  await assertNoCount(page.locator('.notice.error'));

  const connectionsBeforeTabChange = observation.connectionUrls.length;
  await openAppRoute(page, 'operator', 'usage');
  await openAppRoute(page, 'operator', 'requests');
  await eventually(
    () => assert.ok(observation.connectionUrls.length > connectionsBeforeTabChange),
    10_000,
    'realtime stream did not reconnect after returning to the traffic tab',
  );
  const afterAbort = new URL(observation.connectionUrls.at(-1)!);
  assert.equal(afterAbort.searchParams.get('after_event_id'), observation.finalCursorId);
  await assertNoCount(page.locator('.notice.error'));
  await assertCount(operatorTrafficPanel(page).locator('tbody tr'), observation.finalRowCount);

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
  // Filtering affects the rendered snapshot, not the bounded background
  // stream. Clearing filters therefore preserves the durable replay cursor.
  assert.equal(afterFilterReset.searchParams.get('after_event_id'), observation.finalCursorId);
  assert.ok(Number.isSafeInteger(Number(afterFilterReset.searchParams.get('after_event_at'))));

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
