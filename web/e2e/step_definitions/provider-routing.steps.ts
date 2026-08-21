import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { Given, Then, When } from '@cucumber/cucumber';
import type { Locator, Page } from 'playwright';
import { baseURL, eventually, model, requestJson, runtime, tenant } from '../support/runtime.js';
import type { DogfoodWorld } from '../support/world.js';

import { assertAttribute, assertContains, assertCount, assertExactText, assertNoCount, assertNoHorizontalOverflow, assertNotContains, assertValue, assertVisible, applyUsageFilter, clearStrictUsageFilters, clearUsageFilters, connectOperator, credentialGroupObservations, emptyUsageFixture, groupedModel, localizationUsageFixture, metric, nextStrictUsageUrl, requireStrictUsageObservation, strictDimensionUsageFixture, strictUsageObservations, usageDimension, uuidPattern, type StrictUsageObservation } from './dogfood.support.js';
When('OAuth 服务目录包含 Codex 且管理员以中文连接控制台', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await page.route('**/internal/v1/provider-types', async (route) => {
    const response = await route.fetch();
    const providers = await response.json() as Array<{ id: string }>;
    if (providers.some((provider) => provider.id === 'openai-codex')) { await route.fulfill({ response, json: providers }); return; }
    await route.fulfill({ response, json: [...providers, {
      id: 'openai-codex',
      display_name: 'OpenAI Codex',
      protocols: ['openai'],
      modalities: ['text'],
      config_schema: { type: 'object', additionalProperties: false, properties: {} },
      credential_schema: { type: 'object', additionalProperties: false, properties: {} },
      oauth_adapter: null,
      source: 'built-in',
    }] });
  });
  await page.route('**/internal/v1/oauth/codex/start', async (route) => {
    assert.equal(route.request().method(), 'POST');
    await route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify({
      driver: 'openai-codex',
      verification_url: 'https://auth.openai.com/device',
      user_code: 'SAFE-CODE',
      session_token: 'browser-codex-session',
      expires_at: Date.now() + 600_000,
      poll_after_seconds: 5,
      security_notice: 'only_continue_if_you_started_this_login',
    }) });
  });
  await page.route('**/internal/v1/oauth/codex/poll', async (route) => {
    assert.equal(route.request().method(), 'POST');
    assert.deepEqual(route.request().postDataJSON(), { session_token: 'browser-codex-session' });
    await route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify({ status: 'pending' }) });
  });
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
  await assertVisible(onboarding.getByRole('button', { name: 'API 凭据', exact: true }));
  await assertVisible(onboarding.getByRole('button', { name: '账户授权', exact: true }));
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
  await onboarding.getByRole('button', { name: '账户授权', exact: true }).click();
  await assertContains(onboarding.getByLabel('服务提供商'), 'Cursor');

  await page.getByRole('tab', { name: '模型路由', exact: true }).click();
  const routeRow = page.locator('tbody tr').filter({ hasText: model });
  await assertContains(routeRow, 'Browser mock upstream');
  await routeRow.getByRole('button', { name: '编辑', exact: true }).click();
  const routeEditor = page.locator('.inline-editor');
  const synchronizedModels = page.waitForResponse((response) => response.url().includes(`/internal/v1/upstreams/${seed.upstreamId}/models/sync`) && response.request().method() === 'POST');
  await routeEditor.getByRole('button', { name: '同步模型', exact: true }).click();
  assert.equal((await synchronizedModels).status(), 200);
  await assertContains(routeEditor.locator('.catalog-status'), '已同步 1 个候选提供商的模型目录');
  const catalogSearch = page.waitForResponse((response) => {
    const url = new URL(response.url());
    return url.pathname === '/internal/v1/upstream-models' && url.searchParams.get('q') === 'mock-provider-model-v2';
  });
  await routeEditor.getByLabel('上游模型').fill('mock-provider-model-v2');
  const catalogResponse = await catalogSearch;
  assert.equal(catalogResponse.status(), 200);
  assert.equal(new URL(catalogResponse.url()).searchParams.get('account_ids'), seed.upstreamId);
  const catalog = await catalogResponse.json() as { data: Array<{ id: string }> };
  assert.ok(catalog.data.some((catalogModel) => catalogModel.id === 'mock-provider-model-v2'));
  await routeEditor.locator('.model-options').getByRole('option').filter({ hasText: 'mock-provider-model-v2' }).click();
  const updatedRoute = page.waitForResponse((response) => response.url().endsWith(`/internal/v1/model-routes/${seed.routeId}`) && response.request().method() === 'PUT');
  await routeEditor.getByRole('button', { name: '保存', exact: true }).click();
  const updatedRouteResponse = await updatedRoute;
  assert.equal(updatedRouteResponse.status(), 200, await updatedRouteResponse.text());
  await assertContains(page.getByRole('status'), '路由已更新');
  await assertContains(routeRow, 'mock-provider-model-v2');
  await routeRow.getByRole('button', { name: '停用', exact: true }).click();
  await assertContains(routeRow, '已停用');
  await routeRow.getByRole('button', { name: '启用', exact: true }).click();
  await assertContains(routeRow, '已启用');
});

Then('中英文新增上游使用面向操作的产品文案', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await page.getByRole('tab', { name: '上游提供商', exact: true }).click();
  const onboarding = page.locator('.provider-onboarding');
  await assertVisible(page.getByRole('heading', { name: '上游服务', exact: true }));
  await assertContains(page.locator('.provider-list'), '连接并管理模型服务。');
  await assertVisible(onboarding.getByRole('button', { name: 'API 凭据', exact: true }));
  await onboarding.getByRole('button', { name: '账户授权', exact: true }).click();
  await assertVisible(onboarding.getByLabel('服务提供商'));
  await assertContains(onboarding.getByLabel('服务提供商'), 'Cursor');
  await assertContains(onboarding.getByLabel('服务提供商'), 'OpenAI Codex');
  await onboarding.getByLabel('服务提供商').selectOption('provider:openai-codex');
  await onboarding.getByLabel('上游名称').fill('codex-primary');
  await onboarding.getByRole('button', { name: '开始登录', exact: true }).click();
  await assertContains(onboarding.getByRole('status'), '仅当这次登录由你刚刚发起时');
  await assertContains(onboarding.getByRole('status'), 'SAFE-CODE');
  await assertAttribute(onboarding.getByRole('link', { name: '打开授权页', exact: true }), 'href', 'https://auth.openai.com/device');
  await assertVisible(onboarding.getByRole('button', { name: '检查授权结果', exact: true }));
  const polledCodex = page.waitForResponse((response) => response.url().endsWith('/internal/v1/oauth/codex/poll') && response.request().method() === 'POST');
  await onboarding.getByRole('button', { name: '检查授权结果', exact: true }).click();
  assert.equal((await polledCodex).status(), 200);
  await assertNotContains(page.locator('body'), 'CPA');
  await assertNotContains(page.locator('body'), 'Bridge');
  await assertNotContains(page.locator('body'), '订阅桥接');
  await assertNotContains(page.locator('body'), '插件 OAuth Adapter');

  await page.locator('.rail .language-toggle').click();
  await assertAttribute(page.locator('html'), 'lang', 'en');
  await assertVisible(page.getByRole('heading', { name: 'Upstream services', exact: true }));
  await assertContains(page.locator('.provider-list'), 'Connect and manage model services.');
  await assertVisible(onboarding.getByRole('button', { name: 'API credential', exact: true }));
  await assertVisible(onboarding.getByRole('button', { name: 'Account authorization', exact: true }));
  await assertVisible(onboarding.getByLabel('Service provider'));
  await assertContains(onboarding.getByLabel('Service provider'), 'Cursor');
  await assertContains(onboarding.getByLabel('Service provider'), 'OpenAI Codex');
  await assertContains(onboarding.getByRole('status'), 'Continue on OpenAI only if you just started this login.');
  await assertNotContains(page.locator('body'), 'CPA');
  await assertNotContains(page.locator('body'), 'Bridge');
  await assertNotContains(page.locator('body'), 'Subscription bridge');
  await assertNotContains(page.locator('body'), 'Plugin OAuth adapter');
});

When('管理员用键盘创建提供商组和路由组', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const seed = runtime.requireSeed();
  await page.getByRole('tab', { name: '模型路由', exact: true }).click();
  await assertVisible(page.locator('.group-manager[data-group-kind="provider"]'));
  await assertVisible(page.locator('.group-manager[data-group-kind="route"]'));
  const providerGroups = page.locator('.group-manager[data-group-kind="provider"]');
  await providerGroups.getByLabel('组名称').first().fill('主力提供商');
  const createdProviderGroup = page.waitForResponse((response) => response.url().endsWith('/internal/v1/provider-groups') && response.request().method() === 'POST');
  await providerGroups.getByRole('button', { name: '创建组', exact: true }).click();
  const providerGroupResponse = await createdProviderGroup;
  assert.equal(providerGroupResponse.status(), 201);
  const providerGroup = await providerGroupResponse.json() as { id: string };
  const memberInput = providerGroups.getByRole('combobox', { name: '提供商成员', exact: true });
  await memberInput.fill('Browser mock');
  await memberInput.press('ArrowDown');
  await memberInput.press('Enter');
  await assertContains(providerGroups.locator('.selection-chip'), 'Browser mock upstream');
  await memberInput.press('Backspace');
  await assertNoCount(providerGroups.locator('.selection-chip'));
  await memberInput.fill('Browser mock');
  await memberInput.press('Enter');
  await memberInput.press('Escape');
  await assertAttribute(memberInput, 'aria-expanded', 'false');
  const savedProviderMembers = page.waitForResponse((response) => response.url().includes('/internal/v1/provider-groups/') && response.url().endsWith('/members') && response.request().method() === 'PUT');
  await providerGroups.getByRole('button', { name: '保存成员', exact: true }).click();
  const providerMembersResponse = await savedProviderMembers;
  assert.equal(providerMembersResponse.status(), 200);
  const providerMembersPayload = providerMembersResponse.request().postDataJSON() as Record<string, unknown>;
  assert.deepEqual(Object.keys(providerMembersPayload).sort(), ['expected_updated_at', 'member_ids', 'tenant_external_id']);
  assert.deepEqual(providerMembersPayload.member_ids, [seed.upstreamId]);

  await connectOperator(this, 'dark');
  await page.getByRole('tab', { name: '模型路由', exact: true }).click();
  const reloadedProviderGroups = page.locator('.group-manager[data-group-kind="provider"]');
  await assertContains(reloadedProviderGroups, '主力提供商');
  await assertContains(reloadedProviderGroups.locator('.selection-chip'), 'Browser mock upstream');

  const routeEditor = page.locator('article.form-panel').filter({ has: page.getByRole('heading', { name: '创建模型路由', exact: true }) });
  await routeEditor.getByLabel('公开模型').fill(groupedModel);
  const includeProviders = routeEditor.getByRole('combobox', { name: '包含提供商组', exact: true });
  await includeProviders.fill('主力');
  await includeProviders.press('Enter');
  const groupedCatalog = page.waitForResponse((response) => {
    const url = new URL(response.url());
    return url.pathname === '/internal/v1/upstream-models'
      && url.searchParams.get('include_provider_group_ids') === providerGroup.id
      && url.searchParams.get('q') === 'mock-provider-model';
  });
  await routeEditor.getByLabel('上游模型').fill('mock-provider-model');
  const groupedCatalogResponse = await groupedCatalog;
  assert.equal(groupedCatalogResponse.status(), 200);
  const groupedCatalogBody = await groupedCatalogResponse.json() as { data: Array<{ id: string }> };
  assert.ok(groupedCatalogBody.data.some((catalogModel) => catalogModel.id === 'mock-provider-model'));
  const createRouteButton = routeEditor.getByRole('button', { name: '创建路由', exact: true });
  await eventually(async () => assert.equal(await createRouteButton.isEnabled(), true), 10_000, 'group-only route did not become valid after catalog search');
  const routeGroupInput = routeEditor.getByRole('combobox', { name: '所属路由组', exact: true });
  await routeGroupInput.fill('默认路由');
  const routeGroupListId = await routeGroupInput.getAttribute('aria-controls');
  assert.ok(routeGroupListId);
  await assertContains(routeEditor.locator(`#${routeGroupListId}`).getByRole('option'), '创建路由组“默认路由”');
  await routeGroupInput.press('Enter');
  const exactCredentials = routeEditor.getByRole('combobox', { name: '授权给具体凭据', exact: true });
  await exactCredentials.fill('Browser E2E credential');
  await exactCredentials.press('Enter');
  await exactCredentials.press('Escape');
  const createdRoute = page.waitForResponse((response) => response.url().endsWith('/internal/v1/model-routes') && response.request().method() === 'POST');
  await createRouteButton.click();
  const createdRouteResponse = await createdRoute;
  assert.equal(createdRouteResponse.status(), 201);
  const createdRoutePayload = createdRouteResponse.request().postDataJSON() as Record<string, unknown>;
  assert.equal(Object.hasOwn(createdRoutePayload, 'upstream_account_id'), false);
  assert.deepEqual(createdRoutePayload.upstream_account_ids, []);
  assert.deepEqual(createdRoutePayload.included_provider_group_ids, [providerGroup.id]);
  assert.deepEqual(createdRoutePayload.route_group_names, ['默认路由']);
  assert.deepEqual(createdRoutePayload.granted_credential_ids, [seed.clientKeyId]);
  assert.equal(createdRoutePayload.custom_model_confirmed, false);
  const routeRow = page.locator('tbody tr').filter({ hasText: groupedModel });
  await assertVisible(routeRow);

  await connectOperator(this, 'dark');
  await page.getByRole('tab', { name: '模型路由', exact: true }).click();
  const reloadedRouteGroups = page.locator('.group-manager[data-group-kind="route"]');
  await assertContains(reloadedRouteGroups, '默认路由');
  await assertContains(reloadedRouteGroups.locator('.selection-chip'), groupedModel);
});

Then('提供商组参与路由候选而路由组参与凭据授权', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const seed = runtime.requireSeed();
  await page.getByRole('tab', { name: '凭据管理', exact: true }).click();
  const credential = page.locator('.managed-resource').filter({ hasText: seed.clientKeyId });
  const openedRouting = page.waitForResponse((response) => response.url().includes(`/internal/v1/keys/${seed.clientKeyId}/routing`) && response.request().method() === 'GET');
  await credential.getByRole('button', { name: '路由权限', exact: true }).click();
  const openedRoutingResponse = await openedRouting;
  assert.equal(openedRoutingResponse.status(), 200, `${openedRoutingResponse.request().method()} ${openedRoutingResponse.url()} ${await openedRoutingResponse.text()}`);
  const routing = credential.locator('.routing-editor');
  await assertVisible(routing);
  const routeGroupInput = routing.getByRole('combobox', { name: '路由组', exact: true });
  await routeGroupInput.fill('默认路由');
  await routeGroupInput.press('Enter');
  const routingSaved = page.waitForResponse((response) => response.url().endsWith(`/internal/v1/keys/${seed.clientKeyId}/routing`) && response.request().method() === 'PUT');
  await routing.getByRole('button', { name: '保存', exact: true }).click();
  const routingSavedResponse = await routingSaved;
  assert.equal(routingSavedResponse.status(), 200);
  const routingPayload = routingSavedResponse.request().postDataJSON() as Record<string, unknown>;
  assert.equal(Object.hasOwn(routingPayload, 'expected_updated_at'), false);
  assert.equal(typeof routingPayload.expected_grant_revision, 'number');
  await assertContains(routing, '默认路由');
  await assertContains(routing, '当前共可使用');

  const credentialRoutingPath = `/internal/v1/keys/${seed.clientKeyId}/routing?tenant_external_id=${encodeURIComponent(tenant)}`;
  const current = await requestJson<{ route_ids: string[]; grant_revision: number }>(credentialRoutingPath, { credential: seed.serviceCredential });
  await requestJson(`/internal/v1/keys/${seed.clientKeyId}/routing`, {
    method: 'PUT',
    credential: seed.serviceCredential,
    body: { tenant_external_id: tenant, route_ids: current.route_ids, route_group_ids: [], expected_grant_revision: current.grant_revision },
  });
  const staleSave = page.waitForResponse((response) => response.url().endsWith(`/internal/v1/keys/${seed.clientKeyId}/routing`) && response.request().method() === 'PUT');
  await routing.getByRole('button', { name: '保存', exact: true }).click();
  assert.equal((await staleSave).status(), 409);
  await assertContains(page.getByRole('alert'), '路由权限已被其他操作修改，已重新加载最新内容。');
  await assertNotContains(routing, '默认路由');
  await routeGroupInput.fill('默认路由');
  await routeGroupInput.press('Enter');
  const retrySave = page.waitForResponse((response) => response.url().endsWith(`/internal/v1/keys/${seed.clientKeyId}/routing`) && response.request().method() === 'PUT');
  await routing.getByRole('button', { name: '保存', exact: true }).click();
  assert.equal((await retrySave).status(), 200);
});

When('管理员创建凭据组并按组筛选凭据', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const seed = runtime.requireSeed();
  const [routing, models] = await Promise.all([
    requestJson(`/internal/v1/keys/${seed.clientKeyId}/routing?tenant_external_id=${encodeURIComponent(tenant)}`, { credential: seed.serviceCredential }),
    requestJson('/v1/models', { credential: seed.clientCredential }),
  ]);
  credentialGroupObservations.set(this, { routing, models });
  const credentialGroups = page.locator('.group-manager[data-group-kind="credential"]');
  await assertNoCount(page.locator('.group-manager[data-group-kind="provider"]'));
  await assertNoCount(page.locator('.group-manager[data-group-kind="route"]'));
  await credentialGroups.getByLabel('组名称').first().fill('测试凭据');
  const createdCredentialGroup = page.waitForResponse((response) => response.url().endsWith('/internal/v1/credential-groups') && response.request().method() === 'POST');
  await credentialGroups.getByRole('button', { name: '创建组', exact: true }).click();
  assert.equal((await createdCredentialGroup).status(), 201);
  const memberInput = credentialGroups.getByRole('combobox', { name: '凭据成员', exact: true });
  await memberInput.fill(seed.clientKeyId);
  await memberInput.press('ArrowDown');
  await memberInput.press('Enter');
  const savedMembers = page.waitForResponse((response) => response.url().includes('/internal/v1/credential-groups/') && response.url().endsWith('/members') && response.request().method() === 'PUT');
  await credentialGroups.getByRole('button', { name: '保存成员', exact: true }).click();
  const credentialMembersResponse = await savedMembers;
  assert.equal(credentialMembersResponse.status(), 200);
  const credentialMembersPayload = credentialMembersResponse.request().postDataJSON() as Record<string, unknown>;
  assert.deepEqual(Object.keys(credentialMembersPayload).sort(), ['expected_updated_at', 'member_ids', 'tenant_external_id']);
  assert.deepEqual(credentialMembersPayload.member_ids, [seed.clientKeyId]);
  await connectOperator(this, 'dark');
  await page.getByRole('tab', { name: '凭据管理', exact: true }).click();
  await page.getByLabel('按凭据组筛选').selectOption({ label: '测试凭据' });
  await assertCount(page.locator('.managed-resource').filter({ hasText: seed.clientKeyId }), 1);
  await assertNoCount(page.locator('.managed-resource').filter({ hasText: seed.otherClientKeyId }));
});

Then('凭据组只用于分类且不改变凭据授权或可用模型', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const seed = runtime.requireSeed();
  const before = credentialGroupObservations.get(this);
  assert.ok(before, '应在修改凭据组前记录路由权限');
  const [routingAfter, modelsAfter] = await Promise.all([
    requestJson(`/internal/v1/keys/${seed.clientKeyId}/routing?tenant_external_id=${encodeURIComponent(tenant)}`, { credential: seed.serviceCredential }),
    requestJson('/v1/models', { credential: seed.clientCredential }),
  ]);
  assert.deepEqual(routingAfter, before.routing, '凭据组成员变更不应改变路由授权摘要');
  assert.deepEqual(modelsAfter, before.models, '凭据组成员变更不应改变可用模型');
  await page.getByLabel('按凭据组筛选').selectOption('all');
  const credential = page.locator('.managed-resource').filter({ hasText: seed.clientKeyId });
  await credential.getByRole('button', { name: '路由权限', exact: true }).click();
  const routing = credential.locator('.routing-editor');
  await assertVisible(routing);
  await assertContains(routing, '当前共可使用');
  await assertContains(routing, '默认路由');
  await assertNotContains(routing, '测试凭据');
  const routeGroupInput = routing.getByRole('combobox', { name: '路由组', exact: true });
  await routeGroupInput.fill('不存在的权限组');
  await assertNotContains(routing.locator('.combobox-popover'), '创建');
  await routeGroupInput.press('Escape');
  await page.getByRole('tab', { name: '模型路由', exact: true }).click();
  const routeRow = page.locator('tbody tr').filter({ hasText: model });
  await routeRow.getByRole('button', { name: '编辑', exact: true }).click();
  const routeEditor = page.locator('.inline-editor.form-panel');
  await assertNotContains(routeEditor, '测试凭据');
  await assertNoCount(routeEditor.getByLabel('凭据组'));
});
