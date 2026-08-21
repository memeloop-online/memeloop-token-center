import assert from 'node:assert/strict';
import { Then, When } from '@cucumber/cucumber';
import type { Locator, Page } from 'playwright';
import { liveRuntime } from '../support/runtime.js';
import type { LiveWorld } from '../support/world.js';

When('只读验收以中文暗色主题连接操作台', async function (this: LiveWorld) {
  const page = this.requirePage();
  const configuration = liveRuntime.requireConfiguration();
  await this.open(configuration.controlURL, '/operator', 'zh-CN', 'dark');
  await page.getByLabel('服务凭据', { exact: true }).fill(configuration.serviceCredential);
  await page.getByRole('button', { name: '连接', exact: true }).click();
  await page.getByRole('tab', { name: '请求统计', exact: true }).click();
  await page.getByRole('button', { name: '最近 30 天', exact: true }).click();
  await expectUsageLoaded(page);
});

Then('操作台 favicon 和暗色主题正确', async function (this: LiveWorld) {
  const page = this.requirePage();
  assert.equal(await page.locator('html').getAttribute('data-theme'), 'dark');
  assert.equal(await page.locator('meta[name="theme-color"]').getAttribute('content'), '#071014');
  assert.equal(await page.locator('link[rel="icon"]').getAttribute('href'), '/ui-assets/token-center-icon-32.png');
  await page.locator('.brand-mark img').waitFor({ state: 'visible' });
  assert.ok(await page.locator('.brand-mark img').evaluate((image) => (image as HTMLImageElement).naturalWidth > 0));
});

Then('最近三十天请求统计的七个视图均可读取', async function (this: LiveWorld) {
  const page = this.requirePage();
  for (const name of ['总览', '趋势分析', '模型分析', '客户端凭据分析', '会话分析', '上游凭证分析', '用量热力图']) {
    const tab = page.getByRole('tab', { name, exact: true });
    await tab.click();
    assert.equal(await tab.getAttribute('aria-selected'), 'true');
    await page.locator('.usage-tab-panel').waitFor({ state: 'visible' });
  }
  await page.getByRole('tab', { name: '总览', exact: true }).click();
});

Then('中文请求数按万或亿显示并保留精确值', async function (this: LiveWorld) {
  const value = metric(this.requirePage(), '请求数').locator('strong span');
  const text = (await value.textContent())?.trim() ?? '';
  assert.match(text, /^\d+(?:\.\d+)?(?:万亿|亿|万)$/);
  assert.match(await value.getAttribute('title') ?? '', /^\d{1,3}(?:,\d{3})+$/);
});

When('只读验收切换为英文亮色主题', async function (this: LiveWorld) {
  const page = this.requirePage();
  await page.locator('.rail').getByRole('button', { name: 'English', exact: true }).click();
  await page.locator('.rail').getByRole('button', { name: 'Switch to light theme', exact: true }).click();
});

Then('英文请求数使用三位分隔且主题色正确', async function (this: LiveWorld) {
  const page = this.requirePage();
  const text = ((await metric(page, 'Requests').locator('strong span').textContent()) ?? '').trim();
  assert.match(text, /^\d{1,3}(?:,\d{3})+$/);
  assert.equal(await page.locator('html').getAttribute('lang'), 'en');
  assert.equal(await page.locator('html').getAttribute('data-theme'), 'light');
  assert.equal(await page.locator('meta[name="theme-color"]').getAttribute('content'), '#f4f7f5');
});

Then('操作员列表和请求详情均不泄漏上游凭据金丝雀', async function (this: LiveWorld) {
  const page = this.requirePage();
  await page.getByRole('tab', { name: '实时请求', exact: true }).click();
  const detailButton = page.locator('table').getByRole('button', { name: /请求详情$/ }).first();
  await detailButton.waitFor({ state: 'visible', timeout: 60_000 });
  await this.assertProviderSecretAbsent(['/internal/v1/upstreams', '/internal/v1/requests']);
  await detailButton.click();
  await page.getByRole('dialog').waitFor({ state: 'visible' });
  await this.assertProviderSecretAbsent(['/internal/v1/requests/']);
  await page.getByRole('dialog').getByRole('button', { name: '关闭', exact: true }).click();
});

When('只读验收使用旧客户端凭据打开自助门户', async function (this: LiveWorld) {
  const page = this.requirePage();
  const configuration = liveRuntime.requireConfiguration();
  await this.open(configuration.gatewayURL, '/portal', 'zh-CN', 'light');
  await page.getByLabel('客户端凭据', { exact: true }).fill(configuration.clientCredential);
  await page.getByRole('button', { name: '载入', exact: true }).click();
  await page.locator('.key-summary').waitFor({ state: 'visible' });
});

Then('自助门户返回预期稳定凭据主键', async function (this: LiveWorld) {
  const configuration = liveRuntime.requireConfiguration();
  assert.equal((await this.requirePage().locator('.key-summary code').textContent())?.trim(), configuration.expectedKeyId);
});

Then('自助门户至少显示一条历史请求', async function (this: LiveWorld) {
  const rows = this.requirePage().locator('.self-history tbody tr');
  await rows.first().waitFor({ state: 'visible' });
  assert.ok(await rows.count() > 0, 'the stable legacy credential must retain request history');
});

Then('公网网关健康检查可读', async function (this: LiveWorld) {
  const response = await this.navigate(liveRuntime.requireConfiguration().gatewayURL, '/healthz');
  assert.equal(response?.status(), 200);
});

Then('公网网关不暴露操作台和内部 API', async function (this: LiveWorld) {
  const gatewayURL = liveRuntime.requireConfiguration().gatewayURL;
  for (const path of ['/operator', '/internal/v1/tenants']) {
    const response = await this.navigate(gatewayURL, path);
    assert.equal(response?.status(), 404, `${path} must not be routed by the public gateway`);
  }
});

function metric(page: Page, label: string): Locator {
  return page.locator('.usage-metrics .metric').filter({ has: page.locator('span').filter({ hasText: new RegExp(`^${label}$`) }) });
}

async function expectUsageLoaded(page: Page): Promise<void> {
  await page.locator('.usage-tab-panel').waitFor({ state: 'visible', timeout: 60_000 });
  await metric(page, '请求数').waitFor({ state: 'visible' });
  assert.equal(await page.locator('.usage-page .notice.error').count(), 0, 'usage analysis returned an error');
}
