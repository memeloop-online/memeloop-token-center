import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium, type Page } from 'playwright';
import { createIsolatedFixtureServer } from './support/isolated-vite-server.js';
import type { TransportProxyBinding, TransportProxyGroup } from '../src/operator/transportProxyGroups.js';

interface ProxyGroupFixture {
  allowed: boolean;
  groups: TransportProxyGroup[];
  binding: TransportProxyBinding;
  reads: number;
  writes: Array<{ path: string; method: string; body: Record<string, any> }>;
  failure?: { status: number; code: string };
  holdNext: boolean;
  release?: () => void;
  policies: boolean[];
}

declare global {
  interface Window { proxyGroupFixture: ProxyGroupFixture }
}

const privateProxySecret = 'socks5h://fixture-user:privateProxySecret@10.20.30.40:1080';

async function installFixture(page: Page, allowed = true) {
  await page.evaluate(({ allowed, secret }) => {
    const previousFetch = window.fetch;
    const state: ProxyGroupFixture = window.proxyGroupFixture = {
      allowed, groups: [], reads: 0, writes: [], holdNext: false, policies: [],
      binding: {
        account_id: 'account-native', tenant_external_id: 'fixture', binding_version: 0,
        group_id: null, group_version: null, initial_member_id: null, credential_generation: 7, updated_at: 100,
        runtime: { scope: 'this_process', configuration_state: 'unbound', selected_member_id: null, observed_at: 100 },
      },
    };
    window.fetch = async (input, init) => {
      const url = new URL(typeof input === 'string' ? input : input instanceof URL ? input.href : input.url, location.origin);
      if (!url.pathname.includes('transport-proxy-group')) return previousFetch(input, init);
      const method = init?.method ?? 'GET';
      state.policies.push(init?.cache === 'no-store' && init?.credentials === 'omit' && init?.referrerPolicy === 'no-referrer');
      if (!state.allowed) return new Response(JSON.stringify({ error: { code: 'forbidden', message: secret } }), { status: 403 });
      if (method === 'GET') {
        state.reads += 1;
        if (url.searchParams.get('tenant_external_id') !== 'fixture') throw new Error('missing read tenant');
        return new Response(JSON.stringify(url.pathname.endsWith('/transport-proxy-group') ? state.binding : { items: state.groups }));
      }
      const body = JSON.parse(String(init?.body));
      state.writes.push({ path: url.pathname, method, body });
      if (body.tenant_external_id !== 'fixture') throw new Error('missing write tenant');
      if (state.failure) {
        const failure = state.failure; state.failure = undefined;
        return new Response(JSON.stringify({ error: { code: failure.code, message: secret } }), { status: failure.status });
      }
      let response: Response;
      if (url.pathname.endsWith('/transport-proxy-group')) {
        const group = state.groups[0];
        if (body.expected_binding_version !== state.binding.binding_version || body.expected_credential_generation !== state.binding.credential_generation
          || body.expected_updated_at !== state.binding.updated_at || body.expected_group_version !== group.version) {
          return new Response(JSON.stringify({ error: { code: 'proxy_group_binding_conflict' } }), { status: 409 });
        }
        const unbind = method === 'DELETE';
        state.binding = {
          ...state.binding, binding_version: state.binding.binding_version + 1,
          credential_generation: state.binding.credential_generation + 1, updated_at: state.binding.updated_at + 1,
          group_id: unbind ? null : group.id, group_version: unbind ? null : group.version,
          initial_member_id: unbind ? null : body.initial_member_id,
          runtime: { scope: 'this_process', configuration_state: unbind ? 'unbound' : 'pending', selected_member_id: null, observed_at: 101 },
        };
        group.bound_account_count = unbind ? 0 : 1;
        response = new Response(JSON.stringify(state.binding), { status: 202 });
      } else if (method === 'DELETE') {
        state.groups = [];
        response = new Response(null, { status: 204 });
      } else {
        const old = state.groups[0];
        if (method === 'PUT' && body.expected_version !== old.version) return new Response(JSON.stringify({ error: { code: 'proxy_group_version_conflict' } }), { status: 409 });
        const group: TransportProxyGroup = {
          id: 'raw-group-id', tenant_external_id: 'fixture', name: body.name, version: (old?.version ?? 0) + 1,
          bound_account_count: old?.bound_account_count ?? 0,
          members: body.members.map((member: { id?: string; label: string }, index: number) => ({
            id: member.id ?? `raw-member-id-${index}`, label: member.label, scheme: 'socks5h', remote_dns: true, has_auth: true,
          })),
        };
        state.groups = [group];
        response = new Response(JSON.stringify(group), { status: method === 'POST' ? 201 : 200 });
      }
      if (state.holdNext) {
        state.holdNext = false;
        return new Promise<Response>(resolve => { state.release = () => resolve(response); });
      }
      return response;
    };
  }, { allowed, secret: privateProxySecret });
  const probe = await page.evaluate(async () => {
    const response = await window.fetch('/internal/v1/transport-proxy-groups?tenant_external_id=fixture', {
      cache: 'no-store', credentials: 'omit', referrerPolicy: 'no-referrer',
    });
    const body = await response.json();
    return { status: response.status, items: Array.isArray(body.items), code: body.error?.code };
  });
  assert.equal(probe.status, allowed ? 200 : 403, 'fixture permission response must be ready before UI interaction');
  assert.equal(probe.items, allowed);
  if (!allowed) assert.equal(probe.code, 'forbidden');
  await page.getByRole('button', { name: '重新检查管理权限' }).click();
  await page.getByText(allowed ? '已确认当前租户的全局操作员及 providers:write 管理权限。' : '无管理权限：需要具有 providers:write 权限的全局操作员。', { exact: true }).waitFor();
}

async function openManager(page: Page) {
  await page.getByRole('button', { name: '代理组与账号绑定', exact: true }).click();
  await page.getByRole('button', { name: '新建代理组', exact: true }).waitFor();
}

async function enterDraft(page: Page) {
  await page.getByRole('button', { name: '新建代理组', exact: true }).click();
  await page.getByLabel('代理组名称', { exact: true }).fill('研发出口组');
  await page.getByLabel('出口名称', { exact: true }).fill('主出口');
  await page.getByLabel('私网代理地址（必填）', { exact: true }).fill(privateProxySecret);
}

async function confirm(page: Page) {
  await page.locator('.app-confirm-dialog').getByRole('button', { name: '确认继续', exact: true }).click();
}

async function assertSecretsAbsent(page: Page) {
  const serialized = await page.evaluate(() => ({ html: document.documentElement.outerHTML, local: JSON.stringify(localStorage), session: JSON.stringify(sessionStorage) }));
  for (const value of Object.values(serialized)) assert.doesNotMatch(value, /privateProxySecret|fixture-user/);
  assert.doesNotMatch(await page.locator('.transport-proxy-workspace').innerText(), /raw-group-id|raw-member-id|account-native/);
}

test('transport proxy groups: CRUD, binding, validation, CAS, secrets, permissions and bounded exit', { timeout: 120_000 }, async context => {
  const root = fileURLToPath(new URL('..', import.meta.url));
  const server = await createIsolatedFixtureServer({ root, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const origin = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  const errors: string[] = [];
  async function prepare(allowed = true) {
    const page = await browser.newPage();
    page.on('pageerror', error => errors.push(error.message));
    page.on('console', message => { if (message.type() === 'error') errors.push(message.text()); });
    await page.route('**/*', route => {
      const url = new URL(route.request().url());
      return url.origin === origin && !url.pathname.startsWith('/internal/') ? route.continue() : route.abort();
    });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'zh-CN'));
    await page.goto(`${origin}/e2e/fixtures/form-journey.html?workflows&proxy-workflow`);
    await page.getByText('暂时无法确认管理权限，入口已禁用，请稍后重试。', { exact: true }).waitFor();
    await installFixture(page, allowed);
    return page;
  }
  try {
    await context.test('names, private secret handling, CRUD and bind/unbind CAS payloads', async () => {
      const page = await prepare();
      await openManager(page); await enterDraft(page);
      assert.equal(await page.getByLabel('私网代理地址（必填）', { exact: true }).getAttribute('type'), 'password');
      await assertSecretsAbsent(page);
      await page.getByRole('button', { name: '保存代理组', exact: true }).click();
      await page.getByRole('button', { name: '编辑 研发出口组', exact: true }).click();
      assert.equal(await page.getByLabel('替换代理地址（留空保留原值）', { exact: true }).inputValue(), '');
      await page.getByLabel('代理组名称', { exact: true }).fill('共享出口组');
      await page.getByRole('button', { name: '保存代理组', exact: true }).click();
      await page.getByRole('button', { name: '编辑 共享出口组', exact: true }).waitFor();
      await page.getByLabel('账号', { exact: true }).selectOption({ label: '研发订阅' });
      await page.getByLabel('目标代理组', { exact: true }).selectOption({ label: '共享出口组' });
      await page.getByLabel('起始出口', { exact: true }).selectOption({ label: '主出口' });
      await page.getByRole('button', { name: '保存账号绑定', exact: true }).click();
      await page.getByText(/绑定配置已受理，将异步生效/).waitFor();
      assert.equal(await page.locator('.transport-proxy-workspace').getByRole('button', { name: '删除', exact: true }).isEnabled(), false);
      await page.getByLabel('解绑后保留的单代理出口', { exact: true }).selectOption({ label: '主出口' });
      await page.getByRole('button', { name: '解绑并保留所选代理', exact: true }).click(); await confirm(page);
      await page.getByText(/解绑配置已受理/).waitFor();
      await assertSecretsAbsent(page);
      await page.locator('.transport-proxy-workspace').getByRole('button', { name: '删除', exact: true }).click(); await confirm(page);
      await page.getByText('代理组已删除。', { exact: true }).waitFor();
      const fixture = await page.evaluate(() => window.proxyGroupFixture);
      assert.deepEqual(fixture.writes.map(write => write.method), ['POST', 'PUT', 'PUT', 'DELETE', 'DELETE']);
      assert.deepEqual(fixture.writes[0].body, { tenant_external_id: 'fixture', name: '研发出口组', members: [{ label: '主出口', proxy_url: privateProxySecret }] });
      assert.deepEqual(fixture.writes[1].body, { tenant_external_id: 'fixture', name: '共享出口组', expected_version: 1, members: [{ id: 'raw-member-id-0', label: '主出口' }] });
      assert.deepEqual(fixture.writes[2].body, { tenant_external_id: 'fixture', expected_binding_version: 0, expected_credential_generation: 7, expected_updated_at: 100, group_id: 'raw-group-id', expected_group_version: 2, initial_member_id: 'raw-member-id-0' });
      assert.deepEqual(fixture.writes[3].body, { tenant_external_id: 'fixture', expected_binding_version: 1, expected_credential_generation: 8, expected_updated_at: 101, expected_group_version: 2, single_proxy_member_id: 'raw-member-id-0' });
      assert.deepEqual(fixture.writes[4].body, { tenant_external_id: 'fixture', expected_version: 2 });
      assert.ok(fixture.policies.every(Boolean));
      await page.getByRole('button', { name: '关闭并返回供应商', exact: true }).click();
      assert.equal(await page.locator('.transport-proxy-workspace').count(), 0);
      assert.equal(await page.locator('.provider-directory-row').isVisible(), true);
      await page.waitForFunction(() => document.activeElement?.textContent === '代理组与账号绑定');
      await page.close();
    });

    await context.test('400 preserves editable draft; group and binding CAS require reads before retry', async () => {
      const page = await prepare(); await openManager(page); await enterDraft(page);
      await page.evaluate(() => { window.proxyGroupFixture.failure = { status: 400, code: 'invalid_request' }; });
      await page.getByRole('button', { name: '保存代理组', exact: true }).click();
      await page.getByText(/配置无效/).waitFor();
      assert.equal(await page.getByLabel('私网代理地址（必填）', { exact: true }).inputValue(), privateProxySecret);
      assert.equal(await page.getByRole('button', { name: '保存代理组', exact: true }).isEnabled(), true);
      await assertSecretsAbsent(page);
      await page.getByLabel('出口名称', { exact: true }).fill('修正后的出口');
      await page.getByRole('button', { name: '保存代理组', exact: true }).click();
      await page.getByRole('button', { name: '编辑 研发出口组', exact: true }).click();
      await page.getByLabel('代理组名称', { exact: true }).fill('本地修改');
      await page.evaluate(() => { window.proxyGroupFixture.groups[0].version = 3; window.proxyGroupFixture.groups[0].name = '他人修改'; });
      await page.getByRole('button', { name: '保存代理组', exact: true }).click();
      await page.getByText(/代理组已被其他操作更新/).waitFor();
      assert.equal(await page.getByRole('button', { name: '保存代理组', exact: true }).isEnabled(), false);
      await page.getByRole('button', { name: '刷新配置', exact: true }).click(); await confirm(page);
      await page.getByRole('button', { name: '编辑 他人修改', exact: true }).click();
      await page.getByLabel('代理组名称', { exact: true }).fill('确认后的修改');
      await page.getByRole('button', { name: '保存代理组', exact: true }).click();
      await page.getByRole('button', { name: '编辑 确认后的修改', exact: true }).waitFor();
      assert.equal(await page.evaluate(() => window.proxyGroupFixture.writes.at(-1)?.body.expected_version), 3);
      await page.getByLabel('账号', { exact: true }).selectOption({ label: '研发订阅' });
      await page.getByLabel('目标代理组', { exact: true }).selectOption({ label: '确认后的修改' });
      await page.getByLabel('起始出口', { exact: true }).selectOption({ label: '修正后的出口' });
      await page.evaluate(() => { window.proxyGroupFixture.binding.credential_generation = 20; });
      await page.getByRole('button', { name: '保存账号绑定', exact: true }).click();
      await page.getByText(/账号绑定或凭证版本已变化/).waitFor();
      assert.equal(await page.getByRole('button', { name: '保存账号绑定', exact: true }).isEnabled(), false);
      await page.getByRole('button', { name: '刷新配置', exact: true }).click();
      await page.getByLabel('目标代理组', { exact: true }).selectOption({ label: '确认后的修改' });
      await page.getByLabel('起始出口', { exact: true }).selectOption({ label: '修正后的出口' });
      await page.getByRole('button', { name: '保存账号绑定', exact: true }).click();
      await page.getByText(/绑定配置已受理/).waitFor();
      assert.equal(await page.evaluate(() => window.proxyGroupFixture.writes.at(-1)?.body.expected_credential_generation), 20);
      await page.close();
    });

    await context.test('timeout is unknown, late response cannot publish success, pending write can exit', async () => {
      const page = await prepare(); await openManager(page); await enterDraft(page);
      await page.clock.install();
      await page.evaluate(() => { window.proxyGroupFixture.holdNext = true; });
      await page.getByRole('button', { name: '保存代理组', exact: true }).click();
      await page.waitForFunction(() => Boolean(window.proxyGroupFixture.release));
      await page.clock.runFor(15_001);
      await page.getByText(/等待已结束，写入结果未知/).waitFor();
      assert.equal(await page.getByRole('button', { name: '保存代理组', exact: true }).isEnabled(), false);
      await page.evaluate(() => window.proxyGroupFixture.release?.());
      assert.equal(await page.getByText(/代理组配置已保存/).count(), 0);
      assert.equal(await page.evaluate(() => window.proxyGroupFixture.writes.length), 1);
      await page.getByRole('button', { name: '刷新配置', exact: true }).click(); await confirm(page);
      await page.getByText(/已读回当前配置/).waitFor();
      await page.getByRole('button', { name: '编辑 研发出口组', exact: true }).click();
      await page.getByLabel('代理组名称', { exact: true }).fill('等待中修改');
      await page.evaluate(() => { window.proxyGroupFixture.holdNext = true; window.proxyGroupFixture.release = undefined; });
      await page.getByRole('button', { name: '保存代理组', exact: true }).click();
      await page.waitForFunction(() => Boolean(window.proxyGroupFixture.release));
      await page.getByRole('button', { name: '关闭并返回供应商', exact: true }).click();
      await page.getByText(/退出仅停止浏览器等待，不表示服务端取消/).waitFor(); await confirm(page);
      assert.equal(await page.locator('.provider-directory-row').isVisible(), true);
      await page.evaluate(() => window.proxyGroupFixture.release?.());
      await openManager(page);
      await page.getByRole('button', { name: '编辑 等待中修改', exact: true }).waitFor();
      assert.equal(await page.evaluate(() => window.proxyGroupFixture.writes.length), 2);
      await assertSecretsAbsent(page); await page.close();
    });

    await context.test('denied permission disables entry with explanation, not a failing management dialog', async () => {
      const page = await prepare(false);
      await page.getByText('无管理权限：需要具有 providers:write 权限的全局操作员。', { exact: true }).waitFor();
      assert.equal(await page.getByRole('button', { name: '代理组与账号绑定', exact: true }).isEnabled(), false);
      assert.equal(await page.locator('.transport-proxy-workspace').count(), 0);
      assert.doesNotMatch(await page.content(), /privateProxySecret/);
      assert.equal(await page.evaluate(() => window.proxyGroupFixture.writes.length), 0);
      await page.close();
    });
    assert.doesNotMatch(errors.join('\n'), /privateProxySecret|fixture-user/);
    assert.deepEqual(errors, []);
  } finally { await browser.close(); await server.close(); }
});
