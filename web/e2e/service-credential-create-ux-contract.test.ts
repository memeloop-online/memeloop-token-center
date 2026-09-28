import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { readFile, readdir } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

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
    const { serviceCredentialCreatePayload } = await import('/src/operator/pages/ManagementPages.tsx');
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

  await page.locator('details.create-resource > summary').click();
  const form = page.locator('form.service-credential-create');
  const name = form.getByRole('textbox', { name: '名称', exact: true });
  const tenant = form.getByRole('textbox', { name: '租户范围', exact: true });
  assert.equal(await tenant.inputValue(), 'tenant-a');
  assert.equal(await tenant.isEditable(), false);
  await form.getByRole('button', { name: '仅选择只读统计权限' }).click();
  assert.equal(await form.getByRole('checkbox', { name: '读取诊断指标 (metrics:read)' }).isChecked(), true);
  assert.equal(await form.getByRole('checkbox', { name: '读取请求与用量 (requests:read)' }).isChecked(), true);
  assert.equal(await form.getByRole('checkbox', { name: '管理客户端凭据 (keys:write)' }).isChecked(), false);
  await form.locator('[aria-label^="requests:read:"]').focus();
  await page.getByRole('tooltip').filter({ hasText: '读取请求记录、监控快照和用量分析' }).waitFor();

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
  const writes = await page.evaluate(() => window.credentialFixture.requests.filter((request) => request.method !== 'GET'));
  assert.deepEqual(writes.map((request) => request.path), ['/internal/v1/service-tokens']);
  await page.close();
});
