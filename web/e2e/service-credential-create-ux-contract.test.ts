import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { readFile, readdir } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

import { chromium, type Locator } from 'playwright';
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
    createdServiceCredentialBody?: string;
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

test('service credential scopes match the runtime capability set', async () => {
  const source = await readFile(new URL('../src/operator/pages/ManagementPages.tsx', import.meta.url), 'utf8');
  const runtime = await readFile(new URL('../../src/db/credentials/service_tokens.rs', import.meta.url), 'utf8');
  const uiScopes = source.slice(source.indexOf('const serviceCredentialScopeGroups = ['), source.indexOf('const serviceCredentialStatisticsScopes'));
  const runtimeScopes = runtime.match(/const SUPPORTED_SERVICE_SCOPES:.*?= &\[([\s\S]*?)\];/)?.[1];
  assert.ok(runtimeScopes);
  const extract = (value: string) => [...value.matchAll(/"([a-z_]+:[a-z:]+)"|'([a-z_]+:[a-z:]+)'/g)].map((match) => match[1] ?? match[2]).sort();
  assert.deepEqual(extract(uiScopes), extract(runtimeScopes));
});

test('service credential creation validates the draft and issues only a read-only statistics credential', { timeout: 60_000 }, async (context) => {
  const executablePath = await localChromiumExecutable();
  if (!executablePath) return context.skip('a local Chromium runtime is required');
  const server = await createIsolatedFixtureServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0, strictPort: false } });
  await server.listen();
  context.after(() => server.close());
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ executablePath, headless: true });
  context.after(() => browser.close());

  const page = await browser.newPage();
  await page.addInitScript(() => localStorage.setItem('mtc-locale', 'zh-CN'));
  await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/operator-credential-workspace.html?scenario=service-plaintext`);
  await page.getByText('Existing service credential', { exact: true }).waitFor();
  const payloadResults = await page.evaluate(async () => {
    const modulePath = '/src/operator/pages/ManagementPages.tsx';
    const { serviceCredentialCreatePayload } = await import(modulePath);
    return {
      emptyName: serviceCredentialCreatePayload('  ', ['metrics:read'], 'tenant-a') ?? null,
      emptyScopes: serviceCredentialCreatePayload('analytics', [], 'tenant-a') ?? null,
      unknownScope: serviceCredentialCreatePayload('analytics', ['unknown:write'], 'tenant-a') ?? null,
      missingTenant: serviceCredentialCreatePayload('analytics', ['requests:read'], '') ?? null,
      longName: serviceCredentialCreatePayload('统'.repeat(41), ['requests:read'], 'tenant-a') ?? null,
      valid: serviceCredentialCreatePayload('  analytics  ', ['metrics:read', 'requests:read', 'metrics:read'], 'tenant-a'),
    };
  });
  assert.deepEqual(payloadResults, {
    emptyName: null, emptyScopes: null, unknownScope: null, missingTenant: null, longName: null,
    valid: { name: 'analytics', scopes: ['metrics:read', 'requests:read'], tenant_external_id: 'tenant-a' },
  });

  const createPanel = page.locator('section.create-resource');
  assert.equal(await createPanel.locator('details, summary').count(), 0, 'service credential creation uses Fluent disclosure primitives');
  const disclosure = createPanel.getByRole('button', { name: '创建服务凭据表单', exact: true });
  await disclosure.focus();
  await page.keyboard.press('Enter');
  assert.equal(await disclosure.getAttribute('aria-expanded'), 'true');
  assert.equal(await page.getByRole('button', { name: '创建服务凭据', exact: true }).count(), 1, 'the submit button keeps its exact accessible name');
  const form = page.locator('form.service-credential-create');
  const name = form.getByRole('textbox', { name: '名称', exact: true });
  const tenant = form.getByRole('textbox', { name: '租户范围', exact: true });
  assert.equal(await tenant.inputValue(), 'tenant-a');
  assert.equal(await tenant.isEditable(), false);
  const metrics = form.getByRole('checkbox', { name: '读取诊断指标 (metrics:read)' });
  const requests = form.getByRole('checkbox', { name: '读取请求与用量 (requests:read)' });
  await metrics.check();
  await requests.check();
  assert.equal(await metrics.isChecked(), true);
  assert.equal(await requests.isChecked(), true);
  await metrics.uncheck();
  assert.equal(await metrics.isChecked(), false, 'a manually selected scope can be cancelled');
  await metrics.check();
  await requests.uncheck();
  await requests.check();
  await form.getByRole('button', { name: '仅选择只读统计权限' }).click();
  assert.equal(await metrics.isChecked(), true);
  assert.equal(await requests.isChecked(), true);
  assert.equal(await form.getByRole('checkbox', { name: '管理客户端凭据 (keys:write)' }).isChecked(), false);
  const scopeHints = form.locator('.service-credential-scope-option > span[aria-label]');
  assert.ok(await scopeHints.count() > 20, 'each supported permission has a supplemental explanation');
  const assertScopeHintWithKeyboard = async (trigger: Locator, hint: string) => {
    const tooltip = page.getByRole('tooltip', { name: hint, exact: true });
    await trigger.press('Shift+Tab');
    assert.equal(await trigger.evaluate(element => element.contains(document.activeElement)), false, 'Shift+Tab leaves the scope help trigger');
    await tooltip.waitFor({ state: 'hidden' });
    await page.keyboard.press('Tab');
    assert.equal(await trigger.evaluate(element => element === document.activeElement), true, 'Tab focuses the exact scope help trigger');
    await tooltip.waitFor({ state: 'visible' });
    assert.equal(await tooltip.innerText(), hint, 'the focused permission exposes its exact explanation');

    for (let step = 0; step < 2 && await trigger.evaluate(element => element.contains(document.activeElement)); step += 1) {
      await page.keyboard.press('Tab');
    }
    assert.equal(await trigger.evaluate(element => element.contains(document.activeElement)), false, 'Tab leaves the scope help trigger and its checkbox');
    await tooltip.waitFor({ state: 'hidden' });
  };
  for (let index = 0; index < await scopeHints.count(); index += 1) {
    const hint = (await scopeHints.nth(index).getAttribute('aria-label'))?.replace(/^\S+: /, '');
    assert.ok(hint);
    await assertScopeHintWithKeyboard(scopeHints.nth(index), hint);
  }
  await assertScopeHintWithKeyboard(form.locator('[aria-label^="requests:read:"]'), '读取请求记录、监控快照和用量分析；不允许写入。');

  await metrics.uncheck();
  await requests.uncheck();
  await form.getByRole('button', { name: '创建服务凭据' }).click();
  await page.getByText('至少选择一项权限。', { exact: true }).waitFor();
  assert.equal(await page.evaluate(() => window.credentialFixture.requests.filter((request) => request.method === 'POST').length), 0);

  await disclosure.focus();
  await page.keyboard.press('Space');
  assert.equal(await disclosure.getAttribute('aria-expanded'), 'false');
  await page.keyboard.press('Space');
  assert.equal(await disclosure.getAttribute('aria-expanded'), 'true');
  await form.getByRole('button', { name: '仅选择只读统计权限' }).click();
  await form.getByRole('button', { name: '创建服务凭据' }).click();
  await page.getByText('请输入服务凭据名称。', { exact: true }).waitFor();
  assert.equal(await page.evaluate(() => window.credentialFixture.requests.filter((request) => request.method === 'POST').length), 0);

  await page.evaluate(() => {
    const fixtureFetch = window.fetch;
    window.fetch = (input, init) => {
      if (new URL(typeof input === 'string' ? input : input.toString(), location.origin).pathname === '/internal/v1/service-tokens' && init?.method === 'POST') {
        window.createdServiceCredentialBody = String(init.body);
      }
      return fixtureFetch(input, init);
    };
  });
  await name.fill('统计报表集成');
  await form.getByRole('button', { name: '创建服务凭据' }).click();
  await page.waitForFunction(() => Boolean(window.createdServiceCredentialBody));
  assert.deepEqual(JSON.parse(await page.evaluate(() => window.createdServiceCredentialBody!)), {
    name: '统计报表集成', scopes: ['metrics:read', 'requests:read'], tenant_external_id: 'tenant-a',
  });
  await page.evaluate(() => window.credentialFixture.releaseIssue('mts_local_fixture_secret'));
  await page.getByText('mts_local_fixture_secret', { exact: true }).waitFor();
  page.once('dialog', (dialog) => void dialog.accept());
  await page.getByRole('button', { name: '关闭', exact: true }).click();
  await name.fill('');
  await form.getByRole('button', { name: '创建服务凭据' }).click();
  await page.getByText('请输入服务凭据名称。', { exact: true }).waitFor();
  assert.equal(await page.getByRole('status').filter({ hasText: '服务凭据已创建并保存' }).count(), 0, 'a rejected draft clears the previous success message');
  const writes = await page.evaluate(() => window.credentialFixture.requests.filter((request) => request.method !== 'GET'));
  assert.deepEqual(writes.map((request) => request.path), ['/internal/v1/service-tokens'], 'invalid drafts never issue a second request');
  await page.close();
});
