import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { readdir } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test, { type TestContext } from 'node:test';

import { chromium } from 'playwright';
import { createIsolatedFixtureServer } from './support/isolated-vite-server.js';

declare global {
  interface Window {
    credentialFixture: {
      calls: string[];
      requests: Array<{
        method: string;
        path: string;
        cache?: RequestCache;
        credentials?: RequestCredentials;
        referrerPolicy?: ReferrerPolicy;
        hasSignal: boolean;
        body?: string;
      }>;
      releaseIssue: (token: string) => void;
      releaseRoutingResponse: (status: number) => void;
      releaseCredentialScopeA: () => void;
      releaseCredentialCursor: () => void;
      createdObjectUrls: string[];
      revokedObjectUrls: string[];
    };
  }
}

async function localChromiumExecutable() {
  const defaultExecutable = chromium.executablePath();
  if (existsSync(defaultExecutable)) return defaultExecutable;
  const workspaceUserCache = fileURLToPath(new URL('../../../../.cache/ms-playwright', import.meta.url));
  const installations = await readdir(workspaceUserCache, { withFileTypes: true }).catch(() => []);
  for (const installation of installations) {
    if (!installation.isDirectory() || !installation.name.startsWith('chromium-')) continue;
    const executable = join(workspaceUserCache, installation.name, 'chrome-linux64', 'chrome');
    if (existsSync(executable)) return executable;
  }
  return undefined;
}

async function editFixturePage(context: TestContext, query: string) {
  const executablePath = await localChromiumExecutable();
  if (!executablePath) {
    context.skip('a local Chromium runtime is required');
    return undefined;
  }
  const server = await createIsolatedFixtureServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0, strictPort: false } });
  await server.listen();
  context.after(() => server.close());
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ executablePath, headless: true });
  context.after(() => browser.close());
  const page = await browser.newPage();
  context.after(() => page.close());
  await page.addInitScript(() => localStorage.setItem('mtc-locale', 'zh-CN'));
  await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/operator-credential-workspace.html?${query}`);
  await page.getByText('Existing service credential', { exact: true }).waitFor();
  return page;
}

test('service credential edit plan sends only changed sections through PATCH', { timeout: 60_000 }, async (context) => {
  const page = await editFixturePage(context, 'scenario=service-edit');
  if (!page) return;
  const plans = await page.evaluate(async () => {
    const modulePath = '/src/operator/serviceCredentialScopes.ts';
    const { serviceCredentialUpdatePlan } = await import(modulePath);
    const current = { name: 'Existing service credential', scopes: ['keys:read'], tenant_external_id: 'tenant-a' };
    const globalCurrent = { ...current, tenant_external_id: null };
    return {
      unchanged: serviceCredentialUpdatePlan(current, { name: 'Existing service credential', scopes: ['keys:read'], tenant: 'tenant-a' }),
      nameOnly: serviceCredentialUpdatePlan(current, { name: '  统计报表集成  ', scopes: ['keys:read'], tenant: 'tenant-a' }),
      scopesOnly: serviceCredentialUpdatePlan(current, { name: 'Existing service credential', scopes: ['requests:read', 'keys:read', 'keys:read'], tenant: 'tenant-a' }),
      toGlobal: serviceCredentialUpdatePlan(current, { name: 'Existing service credential', scopes: ['keys:read'], tenant: '' }),
      toTenant: serviceCredentialUpdatePlan(globalCurrent, { name: 'Existing service credential', scopes: ['keys:read'], tenant: 'tenant-a' }),
      emptyName: serviceCredentialUpdatePlan(current, { name: '   ', scopes: ['keys:read'], tenant: 'tenant-a' }),
      emptyScopes: serviceCredentialUpdatePlan(current, { name: 'Existing service credential', scopes: [], tenant: 'tenant-a' }),
      unknownScope: serviceCredentialUpdatePlan(current, { name: 'Existing service credential', scopes: ['analytics:read'], tenant: 'tenant-a' }),
    };
  });
  assert.deepEqual(plans, {
    unchanged: { status: 'unchanged' },
    nameOnly: { status: 'ready', payload: { name: '统计报表集成' } },
    scopesOnly: { status: 'ready', payload: { scopes: ['requests:read', 'keys:read'] } },
    toGlobal: { status: 'ready', payload: { tenant_external_id: null } },
    toTenant: { status: 'ready', payload: { tenant_external_id: 'tenant-a' } },
    emptyName: { status: 'invalid' },
    emptyScopes: { status: 'invalid' },
    unknownScope: { status: 'invalid' },
  });
});

test('service credential edit panel shares the create form and saves through PATCH', { timeout: 60_000 }, async (context) => {
  const page = await editFixturePage(context, 'scenario=service-edit');
  if (!page) return;
  const row = page.locator('.credential-compact-row', { hasText: 'Existing service credential' });
  await row.getByRole('button', { name: '编辑', exact: true }).click();

  const form = page.locator('form.service-credential-create');
  await form.getByText('编辑服务凭据', { exact: false }).waitFor();
  const name = form.getByRole('textbox', { name: '名称', exact: true });
  assert.equal(await name.inputValue(), 'Existing service credential');
  const tenant = form.getByRole('combobox', { name: '租户范围', exact: true });
  assert.equal(await tenant.inputValue(), 'tenant-a');
  const tenantOptions = await tenant.locator('option').allTextContents();
  assert.deepEqual(tenantOptions, ['全局范围', 'tenant-a', 'tenant-b'], 'the picker lists an explicit global option and the visible tenants');
  assert.equal(await form.getByRole('checkbox', { name: '读取客户端凭据 (keys:read)' }).isChecked(), true);
  assert.equal(await form.getByRole('checkbox', { name: '读取请求与用量 (requests:read)' }).isChecked(), false);
  const hints = await form.locator('.service-credential-scope-hint').allTextContents();
  assert.ok(hints.length > 20, 'the edit panel reuses the create scope selector with inline capability descriptions');

  await name.fill('统计报表集成');
  await form.getByRole('checkbox', { name: '读取请求与用量 (requests:read)' }).check();
  await form.getByRole('button', { name: '保存修改', exact: true }).click();
  await page.getByText('已更新 统计报表集成。', { exact: true }).waitFor();
  const firstPatch = await page.evaluate(() => window.credentialFixture.requests.filter((request) => request.method === 'PATCH').map((request) => JSON.parse(request.body ?? '{}')));
  assert.deepEqual(firstPatch, [{ name: '统计报表集成', scopes: ['keys:read', 'requests:read'] }], 'an untouched tenant binding is omitted so the server preserves it');
  await page.getByText('统计报表集成', { exact: true }).waitFor();

  await page.locator('.credential-compact-row', { hasText: '统计报表集成' }).getByRole('button', { name: '编辑', exact: true }).click();
  await form.getByRole('button', { name: '保存修改', exact: true }).click();
  await page.getByText('没有需要保存的修改。', { exact: true }).waitFor();
  assert.equal(await page.evaluate(() => window.credentialFixture.requests.filter((request) => request.method === 'PATCH').length), 1, 'an unchanged draft never issues a write');

  await tenant.selectOption({ label: '全局范围' });
  await form.getByRole('button', { name: '保存修改', exact: true }).click();
  await page.getByText('已更新 统计报表集成。', { exact: true }).waitFor();
  const patches = await page.evaluate(() => window.credentialFixture.requests.filter((request) => request.method === 'PATCH').map((request) => JSON.parse(request.body ?? '{}')));
  assert.deepEqual(patches.at(-1), { tenant_external_id: null }, 'selecting the global option sends an explicit null, never an inferred one');
});

test('a real 403 retires service credential write affordances with the capability boundary', { timeout: 60_000 }, async (context) => {
  const page = await editFixturePage(context, 'scenario=service-edit&forbidden');
  if (!page) return;
  const row = page.locator('.credential-compact-row', { hasText: 'Existing service credential' });
  const edit = row.getByRole('button', { name: '编辑', exact: true });
  assert.equal(await edit.isEnabled(), true, 'a selected tenant alone no longer proves writability');
  await edit.click();
  const form = page.locator('form.service-credential-create');
  await form.getByRole('textbox', { name: '名称', exact: true }).fill('被拒绝的改名');
  await form.getByRole('button', { name: '保存修改', exact: true }).click();
  await page.getByText('当前凭据没有服务凭据写入权限', { exact: false }).waitFor();
  await page.getByText('fixture global service_tokens:write required', { exact: true }).waitFor();
  assert.equal(await row.getByRole('button', { name: '编辑', exact: true }).isEnabled(), false, 'the observed 403 disables further edit affordances');
  assert.equal(await row.getByRole('button', { name: '轮换服务凭据', exact: true }).isEnabled(), false, 'the observed 403 disables other write affordances');
  assert.equal(await form.getByRole('button', { name: '保存修改', exact: true }).isEnabled(), false, 'the open editor stops offering the denied save');
  assert.equal(await page.evaluate(() => window.credentialFixture.requests.filter((request) => request.method === 'PATCH').length), 1, 'the denial was one real attempt, not a loop');
});
