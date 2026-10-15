import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium, type Page } from 'playwright';
import { createIsolatedFixtureServer as createServer } from './support/isolated-vite-server.js';

declare global { interface Window { routeScopePayloads: Array<Record<string, unknown>> } }

test('real route forms submit explicit model intent, preserve edit policies and surface server eligibility errors', { timeout: 90_000 }, async () => {
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer!.address();
  assert.ok(address && typeof address !== 'string');
  const origin = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  const artifacts = fileURLToPath(new URL('../e2e-artifacts/ui-system/', import.meta.url));
  await mkdir(artifacts, { recursive: true });
  const errors: string[] = [];
  const open = async (query: string, locale = 'en', width = 390) => {
    const page = await browser.newPage({ viewport: { width, height: 1000 } });
    await page.addInitScript(locale => localStorage.setItem('mtc-locale', locale), locale);
    page.on('pageerror', error => errors.push(error.message));
    await page.route('**/*', route => new URL(route.request().url()).origin === origin && !new URL(route.request().url()).pathname.startsWith('/internal/') ? route.continue() : route.abort());
    await page.goto(`${origin}/e2e/fixtures/route-form-scope.html?${query}`);
    return page;
  };
  const capture = async (page: Page, name: string) => {
    await page.getByRole('combobox', { name: /^(Upstream model|上游模型)$/ }).press('Escape');
    assert.equal(await page.locator('.model-combobox input[type="checkbox"]').count(), 0);
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false);
    await page.screenshot({ path: `${artifacts}/route-model-entry-${name}.png`, fullPage: true });
  };
  const payload = async (page: Page) => {
    await page.waitForFunction(() => window.routeScopePayloads.length === 1);
    const saved = await page.evaluate(() => window.routeScopePayloads[0]);
    assert.equal(saved.tenant_external_id, 'fixture');
    assert.equal(saved.protocol, 'openai');
    assert.equal(Object.keys(saved).some(key => /price|bound|config|credential_generation/.test(key)), false, 'UI must not invent financial or authorization metadata');
    return saved;
  };
  try {
    for (const catalog of ['unknown', 'failed']) {
      const page = await open(`catalog=${catalog}`);
      await page.getByRole('button', { name: 'Create model route', exact: true }).click();
      const accounts = page.getByRole('combobox', { name: 'Upstream accounts', exact: true });
      await accounts.fill('个人账号');
      await page.getByRole('option').filter({ hasText: '个人账号' }).click();
      await accounts.press('Escape');
      const model = page.getByRole('combobox', { name: 'Upstream model', exact: true });
      await model.fill('operator-custom-model');
      await model.press('Escape');
      await page.locator('.catalog-status').getByRole('button', { name: 'Retry', exact: true }).waitFor();
      const create = page.getByRole('button', { name: 'Create route', exact: true });
      assert.equal(await create.isEnabled(), true, catalog);
      assert.equal(await page.locator('.route-protocol-field option[value="generation"]').isDisabled(), true, 'candidate protocol compatibility is retained');
      if (catalog === 'failed') await capture(page, 'manual-en-390');
      await create.click();
      const saved = await payload(page);
      assert.equal(saved.custom_model_confirmed, true);
      assert.equal(saved.upstream_model, 'operator-custom-model');
      assert.deepEqual(saved.upstream_account_ids, ['personal']);
      await page.close();
    }

    const grouped = await open('catalog=failed&groups=1&reject=1', 'zh-CN');
    await grouped.getByRole('button', { name: '创建模型路由', exact: true }).click();
    await grouped.getByRole('button', { name: '按上游组选择与排除', exact: true }).click();
    const include = grouped.getByRole('combobox', { name: '包含提供商组', exact: true });
    await include.fill('自定义供应商组');
    await grouped.getByRole('option').filter({ hasText: '自定义供应商组' }).click();
    await include.press('Escape');
    const exclude = grouped.getByRole('combobox', { name: '排除提供商组', exact: true });
    await exclude.fill('排除团队');
    await grouped.getByRole('option').filter({ hasText: '排除团队' }).click();
    await exclude.press('Escape');
    const groupModel = grouped.getByRole('combobox', { name: '上游模型', exact: true });
    await groupModel.fill('custom-group-model');
    await groupModel.press('Escape');
    const groupSave = grouped.getByRole('button', { name: '创建路由', exact: true });
    assert.equal(await groupSave.isEnabled(), true);
    await groupSave.click();
    const groupPayload = await payload(grouped);
    assert.equal(groupPayload.custom_model_confirmed, true);
    assert.deepEqual(groupPayload.upstream_account_ids, [], 'dynamic groups must not become fixed account lists');
    assert.deepEqual(groupPayload.included_provider_group_ids, ['custom-providers']);
    assert.deepEqual(groupPayload.excluded_provider_group_ids, ['excluded-providers']);
    await grouped.locator('.create-journey [role="alert"]').filter({ hasText: 'the route has no eligible upstream' }).waitFor();
    assert.equal(await groupModel.inputValue(), 'custom-group-model', 'server rejection preserves the draft');
    await capture(grouped, 'group-server-error-zh-CN-390');
    await exclude.fill('自定义供应商组');
    await grouped.getByRole('option').filter({ hasText: '自定义供应商组' }).click();
    await exclude.press('Escape');
    assert.equal(await groupSave.isDisabled(), true, 'excluding every candidate still prevents submission');
    assert.equal(await grouped.evaluate(() => window.routeScopePayloads.length), 1);
    await grouped.close();

    for (const confirmed of [false, true]) {
      const page = await open(`existing=1&catalog=failed&confirmed=${confirmed}&driver=openai-codex`, 'en', 1440);
      await page.getByRole('button', { name: 'Edit', exact: true }).click();
      await page.locator('.catalog-status').getByRole('button', { name: 'Retry', exact: true }).waitFor();
      await page.getByLabel(/Public model/).fill('renamed-route');
      await page.getByRole('button', { name: 'Routing priority', exact: true }).click();
      const priority = page.getByRole('spinbutton', { name: 'Priority', exact: true });
      await priority.fill('7.5');
      assert.equal(await page.getByRole('button', { name: 'Save', exact: true }).isDisabled(), true);
      await priority.fill('7');
      assert.match(await page.locator('.model-combobox').innerText(), /trusted model metadata or a verified reservation bound/);
      const save = page.getByRole('button', { name: 'Save', exact: true });
      assert.equal(await save.isEnabled(), true);
      if (confirmed) await capture(page, 'codex-unchanged-en-1440');
      await save.click();
      const saved = await payload(page);
      assert.equal(saved.custom_model_confirmed, confirmed);
      assert.equal(saved.upstream_model, 'kimi-k2');
      assert.equal(saved.public_model, 'renamed-route');
      assert.equal(saved.priority, 7);
      assert.equal(saved.expected_updated_at, 42);
      assert.equal(saved.expected_grant_revision, 7);
      assert.deepEqual(saved.route_group_ids, ['group-personal']);
      assert.deepEqual(saved.granted_credential_ids, ['00000000-0000-4000-8000-000000000042']);
      await page.close();
    }

    const partial = await open('catalog=partial&groups=1', 'zh-CN', 1440);
    await partial.getByRole('button', { name: '创建模型路由', exact: true }).click();
    await partial.getByRole('button', { name: '按上游组选择与排除', exact: true }).click();
    const groups = partial.getByRole('combobox', { name: '包含提供商组', exact: true });
    await groups.fill('自定义供应商组');
    await partial.getByRole('option').filter({ hasText: '自定义供应商组' }).click();
    await groups.press('Escape');
    const listed = partial.getByRole('combobox', { name: '上游模型', exact: true });
    await listed.fill('kimi-k2');
    await partial.getByRole('option').filter({ hasText: 'kimi-k2' }).first().click();
    const retry = partial.locator('.catalog-status').getByRole('button', { name: '重试', exact: true });
    await retry.click();
    const create = partial.getByRole('button', { name: '创建路由', exact: true });
    assert.equal(await create.isEnabled(), true);
    await capture(partial, 'partial-catalog-zh-CN-1440');
    await create.click();
    assert.equal((await payload(partial)).custom_model_confirmed, false, 'retrying partial coverage must not promote a listed choice to explicit_custom');
    await partial.close();

    for (const query of ['existing=1&foreign=1', 'existing=1&read-only=1']) {
      const page = await open(query);
      assert.equal(await page.getByRole('button', { name: 'Edit', exact: true }).isDisabled(), true);
      assert.equal(await page.evaluate(() => window.routeScopePayloads.length), 0);
      await page.close();
    }
    assert.deepEqual(errors, []);
  } finally { await browser.close(); await server.close(); }
});
