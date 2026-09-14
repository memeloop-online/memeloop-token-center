import assert from 'node:assert/strict';
import { When } from '@cucumber/cucumber';
import { requestJson, runtime, tenant } from '../support/runtime.js';
import type { DogfoodWorld } from '../support/world.js';
import { connectOperator } from './dogfood.support.js';
import { openAppRoute } from './app-route.support.js';

When('管理员通过统一编辑工作区维护真实代理设置', async function (this: DogfoodWorld) {
  const seed = runtime.requireSeed();
  // Synthetic private addresses are only stored, never dialed. No real secret
  // or credential is placed in screenshots, traces, assertion logs or URLs.
  const original = 'socks5h://fixture-user:fixture-password@10.0.0.15:1080';
  const replacement = 'socks5h://fixture-user:updated-fixture-password@10.0.0.16:1080';
  const account = await requestJson<{ id: string }>('/internal/v1/upstreams', {
    method: 'POST', credential: seed.globalServiceCredential,
    body: { tenant_external_id: tenant, name: 'Browser proxy settings account', driver: 'http-json',
      config: { base_url: 'https://example.invalid/v1' },
      credential: { type: 'api_key_proxy', value: 'fixture-only-upstream-key', proxy_url: original, proxy_network_scope: 'private' } },
  });
  await connectOperator(this, 'light', seed.globalServiceCredential, 'visible');
  const page = this.requirePage();
  await openAppRoute(page, 'operator', 'providers');
  const row = page.locator(`[data-upstream-id="${account.id}"]`);
  await row.getByRole('button', { name: '编辑', exact: true }).click();
  const workspace = page.locator('.provider-edit-workspace');
  const current = workspace.locator('.provider-proxy-value input');
  await current.waitFor();
  assert.ok(await current.inputValue() === original, 'the editor shows the original proxy value');
  assert.equal(await current.getAttribute('value'), null);
  await page.context().grantPermissions(['clipboard-read', 'clipboard-write']);
  await workspace.getByRole('button', { name: '复制代理地址', exact: true }).click();
  assert.ok(await page.evaluate(() => navigator.clipboard.readText()) === original, 'copy returns the original proxy value');
  const name = workspace.getByLabel('上游名称', { exact: false });
  await name.fill('Browser proxy settings renamed');
  await workspace.getByRole('button', { name: '配置网络代理', exact: true }).click();
  const proxy = workspace.locator('.upstream-proxy-editor input');
  assert.ok(await proxy.inputValue() === original, 'editing starts with the saved value');
  await proxy.fill(replacement);
  const outerSave = workspace.locator('.rjsf > button[type="submit"]');
  assert.equal(await outerSave.isEnabled(), false);
  const proxySaved = page.waitForResponse(response => new URL(response.url()).pathname === `/internal/v1/upstreams/${account.id}/transport-proxy` && response.request().method() === 'PUT');
  await workspace.getByRole('button', { name: '保存网络代理', exact: true }).click();
  assert.equal((await proxySaved).status(), 200);
  await workspace.getByRole('button', { name: '配置网络代理', exact: true }).waitFor();
  assert.equal(await name.inputValue(), 'Browser proxy settings renamed');
  const providerSaved = page.waitForResponse(response => new URL(response.url()).pathname === `/internal/v1/upstreams/${account.id}` && response.request().method() === 'PUT');
  await outerSave.click();
  assert.equal((await providerSaved).status(), 200, 'outer save uses the updated CAS revision');
  const saved = await requestJson<{ proxy_url: string }>(`/internal/v1/upstreams/${account.id}/transport-proxy?tenant_external_id=${encodeURIComponent(tenant)}`, { credential: seed.globalServiceCredential });
  assert.ok(saved.proxy_url === replacement, 'real Control storage contains the replacement proxy');
});
