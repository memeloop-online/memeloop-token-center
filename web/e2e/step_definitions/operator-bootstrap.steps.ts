import assert from 'node:assert/strict';
import { Then, When } from '@cucumber/cucumber';

import { eventually } from '../support/runtime.js';
import type { DogfoodWorld } from '../support/world.js';

interface ObservedRequest {
  credential: string;
  path: string;
  tenant: string | null;
}

const observations = new WeakMap<DogfoodWorld, ObservedRequest[]>();

function deferred() {
  let resolve!: () => void;
  const promise = new Promise<void>((next) => { resolve = next; });
  return { promise, resolve };
}

When('操作台依次验证单租户、多租户、租户发现失败和快速凭据替换', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const observed: ObservedRequest[] = [];
  observations.set(this, observed);
  const singletonTenants = deferred();
  const slowTenants = deferred();

  await page.route('**/internal/v1/**', async (route) => {
    const request = route.request();
    const url = new URL(request.url());
    const credential = (await request.allHeaders()).authorization?.replace(/^Bearer /, '') ?? '';
    observed.push({ credential, path: url.pathname, tenant: url.searchParams.get('tenant_external_id') });

    if (url.pathname === '/internal/v1/tenants') {
      if (credential === 'singleton-credential') {
        await singletonTenants.promise;
        await route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify([{ external_id: 'singleton-tenant', default_currency: 'USD' }]) });
      } else if (credential === 'multi-credential') {
        await route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify([
          { external_id: 'tenant-a', default_currency: 'USD' },
          { external_id: 'tenant-b', default_currency: 'USD' },
        ]) });
      } else if (credential === 'failed-credential') {
        await route.fulfill({ status: 403, contentType: 'application/json', body: JSON.stringify({ error: { message: 'tenant discovery denied' } }) });
      } else if (credential === 'slow-credential') {
        await slowTenants.promise;
        await route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify([{ external_id: 'stale-tenant', default_currency: 'USD' }]) });
      } else if (credential === 'fast-credential') {
        await route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify([{ external_id: 'fast-tenant', default_currency: 'USD' }]) });
      } else {
        await route.fulfill({ status: 401, contentType: 'application/json', body: JSON.stringify({ error: { message: 'unexpected credential' } }) });
      }
      return;
    }

    if (url.pathname === '/internal/v1/request-events') {
      await route.fulfill({ status: 200, contentType: 'text/event-stream', body: '' });
      return;
    }
    const body = url.pathname === '/internal/v1/schemas' ? '{}' : '[]';
    await route.fulfill({ status: 200, contentType: 'application/json', body });
  });

  await this.open('/operator?view=requests', { theme: 'dark', locale: 'zh-CN' });
  const credentialInput = page.locator('.operator-credential input[type="password"]');
  const connect = page.getByRole('button', { name: '连接', exact: true });

  await eventually(async () => {
    await credentialInput.fill('singleton-credential');
    assert.equal(await credentialInput.inputValue(), 'singleton-credential');
  });
  await eventually(async () => connect.click());
  await eventually(() => assert.equal(observed.filter((value) => value.credential === 'singleton-credential').length, 1));
  assert.equal(observed.find((value) => value.credential === 'singleton-credential')?.path, '/internal/v1/tenants');
  assert.equal(await page.locator('.console-context').textContent(), '载入中…');
  singletonTenants.resolve();
  await eventually(async () => assert.equal(await page.locator('.tenant-picker select').inputValue(), 'singleton-tenant'));
  await eventually(() => assert.ok(observed.filter((value) => value.credential === 'singleton-credential'
    && ['/internal/v1/upstreams', '/internal/v1/requests'].includes(value.path)).length >= 2));
  const singletonResources = observed.filter((value) => value.credential === 'singleton-credential'
    && ['/internal/v1/upstreams', '/internal/v1/requests'].includes(value.path));
  assert.ok(singletonResources.every((value) => value.tenant === 'singleton-tenant'), JSON.stringify(singletonResources));

  await eventually(async () => {
    await credentialInput.fill('multi-credential');
    assert.equal(await credentialInput.inputValue(), 'multi-credential');
  });
  await eventually(async () => connect.click());
  await eventually(async () => assert.equal(await page.locator('.tenant-picker select').inputValue(), ''));
  const multiRequests = observed.filter((value) => value.credential === 'multi-credential');
  assert.equal(multiRequests[0]?.path, '/internal/v1/tenants');
  await eventually(() => assert.ok(observed.filter((value) => value.credential === 'multi-credential'
    && ['/internal/v1/upstreams', '/internal/v1/requests'].includes(value.path)).length >= 2));
  const multiResources = observed.filter((value) => value.credential === 'multi-credential'
    && ['/internal/v1/upstreams', '/internal/v1/requests'].includes(value.path));
  assert.ok(multiResources.every((value) => value.tenant === null), JSON.stringify(multiResources));
  assert.match(await page.locator('.console-context').textContent() ?? '', /全部租户/);

  const failedRequestStart = observed.length;
  await eventually(async () => { await credentialInput.fill('failed-credential'); assert.equal(await credentialInput.inputValue(), 'failed-credential'); });
  await eventually(async () => connect.click());
  await page.getByRole('alert').filter({ hasText: 'tenant discovery denied' }).waitFor();
  const failedRequests = observed.slice(failedRequestStart).filter((value) => value.credential === 'failed-credential');
  assert.deepEqual(failedRequests.map((value) => value.path), ['/internal/v1/tenants']);
  assert.equal(await page.evaluate(() => localStorage.getItem('mtc.operator.service-credential.v1')), 'multi-credential');
  assert.equal(await page.locator('.tenant-picker select').inputValue(), '');

  await eventually(async () => { await credentialInput.fill('slow-credential'); assert.equal(await credentialInput.inputValue(), 'slow-credential'); });
  await eventually(async () => connect.click());
  await eventually(() => assert.ok(observed.some((value) => value.credential === 'slow-credential' && value.path === '/internal/v1/tenants')));
  await eventually(async () => { await credentialInput.fill('fast-credential'); assert.equal(await credentialInput.inputValue(), 'fast-credential'); });
  await eventually(async () => connect.click());
  await eventually(async () => assert.equal(await page.locator('.tenant-picker select').inputValue(), 'fast-tenant'));
  slowTenants.resolve();
  await page.waitForTimeout(100);
  assert.equal(await page.locator('.tenant-picker select').inputValue(), 'fast-tenant');
  assert.equal(await page.evaluate(() => localStorage.getItem('mtc.operator.service-credential.v1')), 'fast-credential');
  assert.deepEqual(observed.filter((value) => value.credential === 'slow-credential').map((value) => value.path), ['/internal/v1/tenants']);
  await eventually(() => assert.ok(observed.filter((value) => value.credential === 'fast-credential'
    && ['/internal/v1/upstreams', '/internal/v1/requests'].includes(value.path)).length >= 2));
  const fastResources = observed.filter((value) => value.credential === 'fast-credential'
    && ['/internal/v1/upstreams', '/internal/v1/requests'].includes(value.path));
  assert.ok(fastResources.every((value) => value.tenant === 'fast-tenant'), JSON.stringify(fastResources));
});

Then('只有当前凭据解析出的安全租户范围会提交到资源 API', function (this: DogfoodWorld) {
  assert.ok(observations.get(this)?.length);
  assert.deepEqual(this.consoleErrors, ['Failed to load resource: the server responded with a status of 403 (Forbidden)']);
  assert.deepEqual(this.failedRequests, []);
});
