import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer } from './support/isolated-vite-server.js';

const labels = {
  'zh-CN': { details: '查看详情', manage: '账号设置与授权操作', rotate: '轮换接入凭据', title: '轮换 rotation@example.org 的接入凭据', back: '返回上一级', close: '关闭并返回上一级', key: '新 API 密钥', submit: '保存并替换凭据', acknowledge: '我已了解替换影响，并确认新凭据适用于此账号。', invalid: '请填写所有必填项，并检查标记出的字段。', failed: '未能确认替换结果。请检查网络和账号状态后再重试。', saved: '新凭据已保存。系统后续将使用新凭据连接此账号。', settings: '编辑 rotation@example.org', permission: '没有替换此账号凭据的权限。请联系管理员确认租户范围和提供商管理权限。', old: '但这里不会在提供商端撤销旧密钥或令牌', oauth: 'OAuth 授权令牌', token: '访问令牌' },
  en: { details: 'View details', manage: 'Account settings and authorization', rotate: 'Rotate access credential', title: 'Rotate access credential for rotation@example.org', back: 'Back to previous page', close: 'Close and return to previous page', key: 'New API key', submit: 'Save and replace credential', acknowledge: 'I understand the impact and confirm that the new credential is intended for this account.', invalid: 'Complete all required fields and check the highlighted fields.', failed: 'Could not confirm the replacement. Check the connection and account status before retrying.', saved: 'The new credential is saved. The system will use it for subsequent connections to this account.', settings: 'Edit rotation@example.org', permission: 'You do not have permission to replace this account’s credential. Ask an administrator to check the tenant scope and provider management permission.', old: 'This does not revoke the old key or token at the provider', oauth: 'OAuth token', token: 'Access token' },
};

test('rotation explains replacement, uses Fluent controls, restores its parent and validates only mocked credentials', { timeout: 120_000 }, async () => {
  const server = await createIsolatedFixtureServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const origin = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  try {
    for (const locale of ['zh-CN', 'en'] as const) {
      const copy = labels[locale];
      const page = await browser.newPage({ viewport: { width: 390, height: 1000 } });
      await page.addInitScript(value => localStorage.setItem('mtc-locale', value), locale);
      const account = { id: 'rotation-fixture', name: 'rotation@example.org', tenant_external_id: 'fixture-a', driver: 'http-json', auth_kind: 'api_key', connection_method: 'api_key', credential_generation: 1, status: 'active', can_rotate: true, can_refresh: false, can_reauthorize: false, route_count: 0, config: { base_url: 'https://provider.example.invalid' }, created_at: 1, updated_at: 2 };
      const writes: { path: string; method: string; body: unknown; idempotency?: string }[] = [];
      let responseStatus = 503;
      await page.route('**/*', async route => {
        const request = route.request(); const url = new URL(request.url());
        assert.equal(url.origin, origin, 'the fixture must never contact a real provider');
        if (!url.pathname.startsWith('/internal/')) return route.continue();
        if (request.method() !== 'GET') {
          writes.push({ path: url.pathname, method: request.method(), body: request.postDataJSON(), idempotency: request.headers()['idempotency-key'] });
          assert.equal(url.pathname, '/internal/v1/upstreams/rotation-fixture/credential');
          assert.equal(request.method(), 'PUT');
          return route.fulfill(responseStatus === 200 ? { json: { ...account, credential_generation: 2, updated_at: 3 } } : { status: responseStatus, json: { error: { message: 'must-not-expose-fixture-secret' } } });
        }
        if (url.pathname === '/internal/v1/upstreams') return route.fulfill({ json: [account] });
        if (url.pathname === '/internal/v1/provider-types') return route.fulfill({ json: [{ id: 'http-json', display_name: 'Fixture provider', source: 'builtin', protocols: ['openai'], modalities: ['text'], config_schema: { type: 'object', properties: { base_url: { type: 'string' } } }, credential_schema: { oneOf: [
          { title: 'API key', type: 'object', additionalProperties: false, required: ['type', 'value'], properties: { type: { const: 'api_key' }, value: { type: 'string', minLength: 1, writeOnly: true }, header: { type: 'string', default: 'authorization' }, prefix: { type: 'string', default: 'Bearer ' } } },
          { title: 'OAuth', type: 'object', additionalProperties: false, required: ['type', 'access_token'], properties: { type: { const: 'oauth' }, access_token: { type: 'string', minLength: 1, writeOnly: true }, refresh_token: { type: 'string', writeOnly: true }, expires_at: { type: 'integer' } } },
        ] } }] });
        return route.fulfill({ json: [] });
      });
      await page.goto(`${origin}/e2e/fixtures/authorization-code.html?full-page`);
      await page.getByRole('button', { name: copy.details, exact: true }).click();
      await page.getByRole('button', { name: copy.manage, exact: true }).click();
      const trigger = page.getByRole('button', { name: copy.rotate, exact: true });
      await trigger.focus(); await page.keyboard.press('Enter');
      const form = page.locator('.upstream-credential-rotation');
      await form.getByRole('heading', { name: copy.title, exact: true }).waitFor();
      assert.equal(await form.getByRole('heading', { name: copy.title }).evaluate(element => element === document.activeElement), true);
      await form.getByText(copy.old, { exact: false }).waitFor();
      await form.getByText(/providers:write/).waitFor();
      assert.equal(await form.getByRole('button', { name: copy.submit }).isDisabled(), true);
      const selector = form.getByRole('combobox');
      await selector.selectOption({ label: copy.oauth });
      await form.getByLabel(copy.token, { exact: false }).first().waitFor();
      assert.equal(await form.locator('input[type="password"]').count(), 2);
      await selector.selectOption({ index: 1 });
      const secret = form.locator('input[type="password"]').first();
      await form.getByRole('checkbox', { name: copy.acknowledge }).focus(); await page.keyboard.press('Space');
      await form.getByRole('button', { name: copy.submit }).click();
      await form.getByRole('alert').filter({ hasText: copy.invalid }).waitFor();
      assert.equal(writes.length, 0);
      await secret.fill('synthetic-rotation-secret');
      assert.ok(await secret.getAttribute('aria-describedby'));
      const describedBy = (await secret.getAttribute('aria-describedby'))!.split(' ');
      assert.equal(await page.evaluate(ids => ids.some(id => document.getElementById(id)?.textContent?.includes('API') || document.getElementById(id)?.textContent?.includes('provider') || document.getElementById(id)?.textContent?.includes('提供商')), describedBy), true);
      for (const width of [390, 1440]) {
        await page.setViewportSize({ width, height: 1000 });
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
        const buttonHeights = await form.locator('.journey-actions .fui-Button').evaluateAll(buttons => buttons.map(button => button.getBoundingClientRect().height));
        assert.equal(new Set(buttonHeights).size, 1);
        const screenshotRoot = fileURLToPath(new URL('../e2e-artifacts/upstream-availability', import.meta.url));
        await mkdir(screenshotRoot, { recursive: true });
        await page.screenshot({ path: `${screenshotRoot}/credential-rotation-${locale}-${width}.png` });
      }
      await secret.press('Enter');
      await form.getByRole('alert').filter({ hasText: copy.failed }).waitFor();
      assert.equal(writes.length, 1);
      assert.deepEqual(writes[0].body, { credential: { type: 'api_key', value: 'synthetic-rotation-secret', header: 'authorization', prefix: 'Bearer ' } });
      assert.ok(writes[0].idempotency);
      assert.equal(await page.getByText('must-not-expose-fixture-secret').count(), 0);
      responseStatus = 403;
      await form.getByRole('button', { name: copy.submit }).click();
      await form.getByRole('alert').filter({ hasText: copy.permission }).waitFor();
      responseStatus = 200;
      await form.getByRole('button', { name: copy.submit }).click();
      await form.waitFor({ state: 'detached' });
      await page.getByText(copy.saved, { exact: true }).waitFor();
      assert.equal(await trigger.isVisible(), true);
      assert.equal(await trigger.evaluate(element => element === document.activeElement), true);
      await trigger.click();
      assert.equal(await form.locator('input[type="password"]').first().inputValue(), '', 'reopening never retains a submitted secret');
      await form.getByRole('button', { name: copy.close }).click();
      assert.equal(await trigger.isVisible(), true);
      await page.locator('[data-inline-edit-trigger="rotation-fixture"]').click();
      await trigger.click();
      await form.getByRole('button', { name: copy.back }).focus(); await page.keyboard.press('Enter');
      await page.getByRole('heading', { name: copy.settings, exact: true }).waitFor();
      assert.equal(await trigger.evaluate(element => element === document.activeElement), true);
      assert.equal(writes.length, 3, 'back and close never write credentials or start OAuth');
      await page.close();
    }
  } finally { await browser.close(); await server.close(); }
});
