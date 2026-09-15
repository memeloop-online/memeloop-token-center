import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer as createServer } from './support/isolated-vite-server.js';
declare global { interface Window { routeScopePayloads: Array<Record<string, unknown>> } }

test('route account metadata, explicit help and access preview preserve the exact saved scope', { timeout: 60000 }, async () => {
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen(); const address = server.httpServer!.address(); assert.ok(address && typeof address !== 'string');
  const origin = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage({ hasTouch: true });
    await page.clock.setFixedTime(new Date('2026-09-14T08:00:00Z'));
    const errors: string[] = []; page.on('pageerror', error => errors.push(error.message));
    await page.route('**/*', route => new URL(route.request().url()).origin === origin && !new URL(route.request().url()).pathname.startsWith('/internal/') ? route.continue() : route.abort());
    await page.goto(`${origin}/e2e/fixtures/route-form-scope.html`);
    await page.getByRole('button', { name: '创建模型路由', exact: true }).click();
    const accounts = page.getByRole('combobox', { name: '上游账号', exact: true });
    await accounts.fill('Kimi Code');
    const accountOptions = page.locator('.multi-combobox-popover [role="option"]');
    await accountOptions.first().waitFor();
    assert.equal(await accountOptions.count(), 2, 'provider display name finds its named accounts');
    assert.equal((await accountOptions.allTextContents()).some(value => value.includes('technical-driver')), false);
    await accountOptions.filter({ hasText: '个人账号' }).click(); await accounts.press('Escape');
    const model = page.getByRole('combobox', { name: '上游模型', exact: true });
    await model.fill('kimi-k2');
    const modelOption = page.getByRole('option').filter({ hasText: 'kimi-k2' }); await modelOption.waitFor();
    assert.match(await page.locator('.shared-model-popover').innerText(), /Kimi Code/);
    await model.press('ArrowDown'); await model.press('Enter'); await model.press('Escape');
    await page.getByLabel(/公开模型/).fill('kimi-personal');
    const groups = page.getByRole('combobox', { name: '所属路由组', exact: true });
    await groups.fill('个人模型组'); await groups.press('Enter'); await groups.press('Escape');
    await groups.fill('待建模型组'); await groups.press('Enter'); await groups.press('Escape');
    const preview = page.getByRole('region', { name: '配置预览', exact: true });
    assert.match(await preview.innerText(), /所属路由组 \(2\)/);
    assert.match(await preview.innerText(), /个人模型组/);
    assert.match(await preview.innerText(), /待建模型组 \(待创建\)/);
    assert.match(await preview.innerText(), /无直接授权；路由组授权单独生效/);
    await page.getByRole('button', { name: '单独授权凭据（0）', exact: true }).click();
    const credentials = page.getByRole('combobox', { name: '授权给具体凭据', exact: true });
    await credentials.fill('00000000-0000-4000-8000-000000000042');
    const credentialOption = page.locator('.multi-combobox-popover [role="option"]').filter({ hasText: '精确查找凭据' }); await credentialOption.waitFor();
    await credentialOption.click(); await credentials.press('Escape');
    assert.match(await preview.innerText(), /直接授权凭据 \(1\)/);
    assert.match(await preview.innerText(), /精确查找凭据/, 'exact-lookup labels outside the initial page reach the preview');
    assert.equal((await preview.innerText()).includes('00000000-0000-4000-8000-000000000042'), false);
    assert.equal((await preview.innerText()).includes('团队账号'), false);
    const priority = page.getByRole('button', { name: '路由优先级', exact: true });
    await priority.click(); const number = page.getByRole('spinbutton', { name: '优先级', exact: true }); await number.fill('7'); await priority.click(); await priority.click(); assert.equal(await number.inputValue(), '7'); await priority.click();
    for (const [width, theme] of [[390, 'light'], [1440, 'dark']] as const) {
      await page.setViewportSize({ width, height: 1000 }); await page.evaluate(theme => { document.documentElement.dataset.theme = theme; }, theme);
      const help = page.getByRole('button', { name: '协议兼容性说明', exact: true });
      // Escape dismisses the detail but retains focus after the previous tap.
      // Re-enter through keyboard navigation so every viewport gets a real focus event.
      await help.focus(); await help.press('Shift+Tab'); await page.keyboard.press('Tab');
      assert.equal(await help.evaluate(element => element === document.activeElement), true);
      await page.getByRole('tooltip').waitFor();
      assert.match(await page.getByRole('tooltip').innerText(), /保存时检查候选上游的协议兼容性/);
      await page.keyboard.press('Escape');
      await help.tap(); const tip = page.getByRole('tooltip'); await tip.waitFor();
      const bounds = await tip.boundingBox(); assert.ok(bounds && bounds.x >= -1 && bounds.x + bounds.width <= width + 1);
      await page.keyboard.press('Escape');
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false);
    }
    assert.equal(await page.evaluate(() => window.routeScopePayloads.length), 0);
    await page.getByRole('button', { name: '创建路由', exact: true }).click();
    await page.waitForFunction(() => window.routeScopePayloads.length === 1);
    const payload = await page.evaluate(() => window.routeScopePayloads[0]);
    assert.deepEqual(payload.upstream_account_ids, ['personal']);
    assert.deepEqual(payload.included_provider_group_ids, []); assert.deepEqual(payload.excluded_provider_group_ids, []);
    assert.deepEqual(payload.route_group_ids, ['group-personal']); assert.deepEqual(payload.route_group_names, ['待建模型组']);
    assert.deepEqual(payload.granted_credential_ids, ['00000000-0000-4000-8000-000000000042']);
    assert.equal(payload.public_model, 'kimi-personal'); assert.equal(payload.upstream_model, 'kimi-k2'); assert.equal(payload.priority, 7);
    assert.deepEqual(errors, []);
  } finally { await browser.close(); await server.close(); }
});
