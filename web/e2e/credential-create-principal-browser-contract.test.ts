import assert from 'node:assert/strict';
import { existsSync, mkdirSync } from 'node:fs';
import { readdir } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

import { chromium } from 'playwright';
import { createIsolatedFixtureServer as createServer } from './support/isolated-vite-server.js';

const webRoot = fileURLToPath(new URL('..', import.meta.url));

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

test('client credential creation names the principal once and binds existing principals on demand', { timeout: 90_000 }, async () => {
  const executablePath = await localChromiumExecutable();
  if (!executablePath) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required for the credential creation CI gate');
    return test.skip('a local Chromium runtime is required for credential creation behavior assertions');
  }
  const server = await createServer({ root: webRoot, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0, strictPort: false } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const origin = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ executablePath, headless: true });
  const artifacts = join(webRoot, 'e2e-artifacts/upstream-availability');
  mkdirSync(artifacts, { recursive: true });
  try {
    for (const locale of ['zh-CN', 'en'] as const) {
      const english = locale === 'en';
      const page = await browser.newPage();
      await page.addInitScript(value => localStorage.setItem('mtc-locale', value), locale);
      await page.goto(`${origin}/e2e/fixtures/operator-credential-workspace.html?scenario=client-form`);
      await page.getByText(english ? 'Research workspace' : '研发工作区', { exact: true }).waitFor();
      const create = page.locator('.create-journey');
      await create.locator(':scope > .journey-heading [data-workspace-toggle]').click();
      const aliasInput = create.locator('#root_alias');
      await aliasInput.waitFor();
      assert.equal(await create.locator('#root_principal_external_id').count(), 0, 'the default form asks for one readable name, not a second identical principal field');
      await create.getByText(english ? /Also names the user/ : /同时作为使用此凭据的/).waitFor();

      const firstAlias = english ? 'Solo client' : '独立客户端';
      await aliasInput.fill(firstAlias);
      const toggle = create.locator(':scope > .journey-heading [data-workspace-toggle]');
      await toggle.click();
      await toggle.click();
      assert.equal(await aliasInput.inputValue(), firstAlias, 'closing the journey preserves the unsaved draft');

      for (const width of [390, 1440] as const) {
        await page.setViewportSize({ width, height: 900 });
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true, 'no horizontal overflow');
        await page.screenshot({ path: join(artifacts, `credential-create-principal-${locale}-${width}.png`), fullPage: true });
      }

      const createdPosts = () => page.evaluate(() => window.credentialFixture.requests.filter(request => request.method === 'POST' && request.path === '/internal/v1/keys'));
      await create.locator('button[type="submit"]').click();
      await page.locator('.one-time').getByText('mtc_fixture_created', { exact: true }).waitFor();
      let posts = await createdPosts();
      assert.equal(posts.length, 1);
      let body = JSON.parse(posts[0].body!);
      assert.equal(body.alias, firstAlias);
      assert.equal(body.principal_external_id, firstAlias, 'the readable name doubles as the principal by default');
      assert.equal(body.tenant_external_id, 'tenant-a');
      page.once('dialog', dialog => dialog.accept());
      await page.locator('.one-time .one-time-close').click();

      // A successful creation closes the journey; reopen it for the next one.
      await toggle.click();
      await aliasInput.fill(english ? 'Research second key' : '研发第二个凭据');
      const bindingToggle = create.getByRole('button', { name: english ? 'Bind an existing principal (optional)' : '绑定已有主体（可选）', exact: true });
      await bindingToggle.click();
      assert.equal(await bindingToggle.getAttribute('aria-expanded'), 'true', 'the optional binding disclosure opens');
      const principal = create.getByRole('combobox', { name: english ? 'Principal' : '用户主体', exact: true });
      await principal.click();
      await page.getByRole('option', { name: 'tenant-a-principal', exact: true }).click();
      assert.equal(await principal.inputValue(), 'tenant-a-principal');
      await page.setViewportSize({ width: 1440, height: 900 });
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true, 'no horizontal overflow');
      await page.screenshot({ path: join(artifacts, `credential-create-principal-${locale}-binding.png`), fullPage: true });
      await create.locator('button[type="submit"]').click();
      await page.locator('.one-time').getByText('mtc_fixture_created', { exact: true }).waitFor();
      posts = await createdPosts();
      assert.equal(posts.length, 2);
      body = JSON.parse(posts[1].body!);
      assert.equal(body.alias, english ? 'Research second key' : '研发第二个凭据');
      assert.equal(body.principal_external_id, 'tenant-a-principal', 'binding reuses the exact existing principal so ownership stays linked');
      page.once('dialog', dialog => dialog.accept());
      await page.locator('.one-time .one-time-close').click();

      // The binding disclosure stays mounted and open across creations.
      await toggle.click();
      await aliasInput.fill(english ? 'Side project' : '旁支项目');
      await principal.fill(english ? 'custom-owner' : '自定义主体');
      assert.equal(await principal.inputValue(), english ? 'custom-owner' : '自定义主体', 'the freeform principal stays as typed');
      await create.locator('button[type="submit"]').click();
      await page.locator('.one-time').getByText('mtc_fixture_created', { exact: true }).waitFor();
      posts = await createdPosts();
      assert.equal(posts.length, 3);
      body = JSON.parse(posts[2].body!);
      assert.equal(body.principal_external_id, english ? 'custom-owner' : '自定义主体', 'a freeform principal still flows through for new owners');
      page.once('dialog', dialog => dialog.accept());
      await page.locator('.one-time .one-time-close').click();
      await toggle.click();
      assert.equal(await principal.inputValue(), '', 'a successful creation resets the optional binding');
      await page.close();
    }

    const duplicate = await browser.newPage();
    await duplicate.addInitScript(() => localStorage.setItem('mtc-locale', 'zh-CN'));
    await duplicate.goto(`${origin}/e2e/fixtures/operator-credential-workspace.html?scenario=client-form&duplicate-alias=1`);
    await duplicate.getByText('研发工作区', { exact: true }).waitFor();
    const duplicateCreate = duplicate.locator('.create-journey');
    await duplicateCreate.locator(':scope > .journey-heading [data-workspace-toggle]').click();
    await duplicateCreate.locator('#root_alias').fill('研发工作区');
    await duplicateCreate.locator('button[type="submit"]').click();
    await duplicate.locator('.create-journey .notice.error').getByText('fixture duplicate alias', { exact: false }).waitFor();
    assert.equal(await duplicateCreate.locator('#root_alias').inputValue(), '研发工作区', 'a rejected duplicate name keeps the draft for correction');
    await duplicate.close();
  } finally {
    await browser.close();
    await server.close();
  }
});
