import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import { mkdir } from 'node:fs/promises';
import test from 'node:test';
import { chromium, type Page } from 'playwright';
import { createIsolatedFixtureServer } from './support/isolated-vite-server.js';
import type { TransportProxyBinding, TransportProxyGroup } from '../src/operator/transportProxyGroups.js';
import { transportProxyGroupCopy } from '../src/operator/transportProxyGroupCopy.js';

interface ProxyGroupFixture {
  allowed: boolean;
  groups: TransportProxyGroup[];
  binding: TransportProxyBinding;
  reads: number;
  accessReads: number;
  accessFailure?: boolean;
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

async function installFixture(page: Page, allowed = true, locale = 'zh-CN') {
  const copy = transportProxyGroupCopy(locale);
  await page.evaluate(({ allowed, secret }) => {
    const previousFetch = window.fetch;
    const state: ProxyGroupFixture = window.proxyGroupFixture = {
      allowed, groups: [], reads: 0, accessReads: 0, writes: [], holdNext: false, policies: [],
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
      if (url.pathname === '/internal/v1/transport-proxy-groups/access') {
        if (method !== 'GET' || url.search) throw new Error('capability must read the credential scope, not the selected tenant');
        state.accessReads += 1;
        if (state.accessFailure) return new Response(JSON.stringify({ error: { message: secret } }), { status: 503 });
        return new Response(JSON.stringify({ can_manage: state.allowed }));
      }
      if (method === 'GET') state.reads += 1;
      if (!state.allowed) return new Response(JSON.stringify({ error: { code: 'forbidden', message: secret } }), { status: 403 });
      if (method === 'GET') {
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
    const response = await window.fetch('/internal/v1/transport-proxy-groups/access', {
      cache: 'no-store', credentials: 'omit', referrerPolicy: 'no-referrer',
    });
    const body = await response.json();
    return { status: response.status, canManage: body.can_manage };
  });
  assert.equal(probe.status, 200, 'fixture self-capability response must be ready before UI interaction');
  assert.equal(probe.canManage, allowed);
  await page.locator('.provider-list .transport-proxy-management-action').getByRole('button', { name: copy.retry, exact: true }).click();
  if (allowed) await page.waitForFunction(label => Array.from(document.querySelectorAll('button')).some(button => button.textContent === label && !button.disabled), copy.manage);
  else await page.getByText(copy.denied, { exact: true }).waitFor();
  assert.equal(await page.getByText('已具备代理组管理权限。', { exact: true }).count(), 0);
  assert.equal(await page.evaluate(() => window.proxyGroupFixture.reads), 0, 'capability checks must not read group or binding data');
}

async function openManager(page: Page) {
  await page.getByRole('button', { name: '管理代理组', exact: true }).click();
  await page.getByRole('button', { name: '新建代理组', exact: true }).waitFor();
}

async function enterDraft(page: Page) {
  await page.getByRole('button', { name: '新建代理组', exact: true }).click();
  const groupName = page.getByRole('textbox', { name: /^代理组名称\s*\*?$/ });
  const memberName = page.getByRole('textbox', { name: /^出口名称\s*\*?$/ });
  await groupName.fill('研发出口组');
  await memberName.fill('主出口');
  assert.equal(await groupName.getAttribute('required'), '');
  assert.equal(await memberName.getAttribute('required'), '');
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

async function assertWorkspaceFocus(page: Page) {
  assert.deepEqual(await page.locator('.transport-proxy-workspace').evaluate(workspace => ({
    containsFocus: workspace.contains(document.activeElement),
    hidden: Boolean(workspace.closest('[aria-hidden="true"], [inert]')),
  })), { containsFocus: true, hidden: false }, 'the open workspace keeps focus and stays accessible after its initiating control is removed');
}

test('transport proxy groups: CRUD, binding, validation, CAS, secrets, permissions and bounded exit', { timeout: 120_000 }, async context => {
  const root = fileURLToPath(new URL('..', import.meta.url));
  const artifacts = fileURLToPath(new URL('../e2e-artifacts/upstream-availability/proxy-groups/', import.meta.url));
  await mkdir(artifacts, { recursive: true });
  const server = await createIsolatedFixtureServer({ root, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const origin = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  const errors: string[] = [];
  let activePage: Page | undefined;
  context.afterEach(async child => {
    if (!activePage || activePage.isClosed()) return;
    const state = await activePage.evaluate(() => ({
      workspaceCount: document.querySelectorAll('.transport-proxy-workspace').length,
      confirmations: Array.from(document.querySelectorAll('dialog[open]')).map(dialog => dialog.textContent),
      buttons: Array.from(document.querySelectorAll('.transport-proxy-workspace button')).map(button => ({
        text: button.textContent, disabled: button.matches(':disabled'),
        hiddenAncestor: button.closest('[aria-hidden="true"], [inert]')?.tagName,
      })),
      alerts: Array.from(document.querySelectorAll('.transport-proxy-workspace [role="alert"]')).map(alert => alert.textContent),
      focus: document.activeElement?.tagName,
      writes: window.proxyGroupFixture.writes.map(write => ({ method: write.method, path: write.path })),
      reads: window.proxyGroupFixture.reads,
    }));
    child.diagnostic(JSON.stringify(state));
    await activePage.close();
  });
  async function prepare(allowed = true, locale = 'zh-CN') {
    const page = await browser.newPage();
    activePage = page;
    page.on('pageerror', error => errors.push(error.message));
    page.on('console', message => { if (message.type() === 'error') errors.push(message.text()); });
    await page.route('**/*', route => {
      const url = new URL(route.request().url());
      return url.origin === origin && !url.pathname.startsWith('/internal/') ? route.continue() : route.abort();
    });
    await page.addInitScript(value => localStorage.setItem('mtc-locale', value), locale);
    await page.goto(`${origin}/e2e/fixtures/form-journey.html?workflows&proxy-workflow`);
    await page.getByText(transportProxyGroupCopy(locale).unavailable, { exact: true }).waitFor();
    await installFixture(page, allowed, locale);
    return page;
  }
  try {
    await context.test('one shared workspace opens from the toolbar and account connection settings without losing context', async () => {
      const page = await prepare();
      const toolbar = page.locator('.provider-list .quota-read-toolbar');
      assert.equal(await toolbar.getByRole('button', { name: '管理代理组', exact: true }).count(), 1);
      assert.equal(await page.locator('.transport-proxy-management-action [role="status"]').count(), 0);
      const accessReads = await page.evaluate(() => window.proxyGroupFixture.accessReads);
      const row = page.locator('.provider-directory-row');
      await row.getByRole('button', { name: '查看详情', exact: true }).click();
      const detailAction = page.locator('.provider-detail-workspace .upstream-connection').getByRole('button', { name: '选择代理组', exact: true });
      await detailAction.click();
      const workspace = page.locator('.transport-proxy-workspace');
      await workspace.getByRole('button', { name: '新建代理组', exact: true }).waitFor();
      assert.equal(await workspace.count(), 1);
      assert.equal(await workspace.getByLabel('账号', { exact: true }).inputValue(), 'account-native');
      assert.equal(await workspace.getByLabel('账号', { exact: true }).locator('option:checked').textContent(), '研发订阅');
      for (const width of [1440, 390]) {
        await page.setViewportSize({ width, height: 1000 });
        const bounds = await page.getByRole('dialog').boundingBox();
        assert.ok(bounds && bounds.x >= -1 && bounds.x + bounds.width <= width + 1);
        await page.screenshot({ path: `${artifacts}/account-group-zh-${width}.png`, fullPage: true });
      }
      await page.setViewportSize({ width: 1440, height: 1000 });
      await page.getByRole('dialog').getByRole('button', { name: '关闭', exact: true }).click();
      await page.waitForFunction(() => document.activeElement?.textContent === '选择代理组');
      assert.equal(await detailAction.evaluate(button => document.activeElement === button), true);
      assert.equal(await page.locator('.provider-detail-workspace').isVisible(), true);
      await row.getByRole('button', { name: '编辑', exact: true }).click();
      const settings = page.locator('.provider-edit-workspace');
      const name = settings.getByLabel('上游名称', { exact: false });
      await name.fill('保留账号修改');
      const settingsAction = settings.locator('.upstream-connection').getByRole('button', { name: '选择代理组', exact: true });
      await settingsAction.click();
      await workspace.getByRole('button', { name: '新建代理组', exact: true }).waitFor();
      assert.equal(await workspace.count(), 1);
      assert.equal(await workspace.getByLabel('账号', { exact: true }).inputValue(), 'account-native');
      await page.getByRole('dialog').getByRole('button', { name: '关闭', exact: true }).click();
      await page.waitForFunction(() => document.activeElement?.textContent === '选择代理组');
      assert.equal(await settingsAction.evaluate(button => document.activeElement === button), true);
      assert.equal(await name.inputValue(), '保留账号修改');
      assert.equal(await page.evaluate(() => window.proxyGroupFixture.accessReads), accessReads);
      assert.equal(await page.evaluate(() => window.proxyGroupFixture.writes.length), 0);
      assert.equal(await page.evaluate(() => window.formJourneyWrites), 0);
      await page.close();
    });

    await context.test('English group workspace explains the feature instead of reporting permission status', async () => {
      const page = await prepare(true, 'en');
      await page.getByRole('button', { name: 'Manage proxy groups', exact: true }).click();
      const workspace = page.locator('.transport-proxy-workspace');
      await workspace.getByRole('button', { name: 'Create proxy group', exact: true }).waitFor();
      assert.match(await workspace.innerText(), /Give an account backup network exits/);
      assert.doesNotMatch(await workspace.innerText(), /代理组|私网代理地址|已具备|permission granted|worklog|raw-group-id/);
      await workspace.getByRole('button', { name: 'Create proxy group', exact: true }).click();
      assert.equal(await workspace.getByLabel('Proxy group name', { exact: false }).count(), 1);
      assert.equal(await workspace.getByLabel('Private proxy address (required)', { exact: true }).getAttribute('type'), 'password');
      await page.setViewportSize({ width: 390, height: 1000 });
      const bounds = await page.getByRole('dialog').boundingBox();
      assert.ok(bounds && bounds.x >= -1 && bounds.x + bounds.width <= 391);
      await page.screenshot({ path: `${artifacts}/create-group-en-390.png`, fullPage: true });
      assert.equal(await page.evaluate(() => window.proxyGroupFixture.writes.length), 0);
      await page.close();
    });

    await context.test('names, private secret handling, CRUD and bind/unbind CAS payloads', async () => {
      const page = await prepare();
      await openManager(page); await enterDraft(page);
      assert.equal(await page.getByLabel('私网代理地址（必填）', { exact: true }).getAttribute('type'), 'password');
      await assertSecretsAbsent(page);
      await page.getByRole('button', { name: '保存代理组', exact: true }).click();
      await page.getByText(/代理组配置已保存/).waitFor();
      await assertWorkspaceFocus(page);
      await page.getByRole('button', { name: '编辑 研发出口组', exact: true }).click();
      assert.equal(await page.getByLabel('替换代理地址（留空保留原值）', { exact: true }).inputValue(), '');
      await page.getByRole('textbox', { name: /^代理组名称\s*\*?$/ }).fill('共享出口组');
      await page.getByRole('button', { name: '保存代理组', exact: true }).click();
      await page.getByRole('button', { name: '编辑 共享出口组', exact: true }).waitFor();
      await page.getByLabel('账号', { exact: true }).selectOption({ label: '研发订阅' });
      await page.getByLabel('目标代理组', { exact: true }).selectOption({ label: '共享出口组' });
      await page.getByLabel('起始出口', { exact: true }).selectOption({ label: '主出口' });
      await page.getByRole('button', { name: '保存账号绑定', exact: true }).click();
      await page.getByText('账号代理组已保存，正在应用新的连接设置。', { exact: true }).waitFor();
      assert.equal(await page.locator('.transport-proxy-workspace').getByRole('button', { name: '删除', exact: true }).isEnabled(), false);
      await page.getByLabel('解绑后保留的单代理出口', { exact: true }).selectOption({ label: '主出口' });
      await page.getByRole('button', { name: '解绑并保留所选代理', exact: true }).click(); await confirm(page);
      await page.getByText('已解除代理组绑定，账号将继续使用所选出口，不会改为直连。', { exact: true }).waitFor();
      await assertWorkspaceFocus(page);
      await assertSecretsAbsent(page);
      await page.locator('.transport-proxy-workspace').getByRole('button', { name: '删除', exact: true }).click(); await confirm(page);
      await page.getByText('代理组已删除。', { exact: true }).waitFor();
      await assertWorkspaceFocus(page);
      const fixture = await page.evaluate(() => window.proxyGroupFixture);
      assert.deepEqual(fixture.writes.map(write => write.method), ['POST', 'PUT', 'PUT', 'DELETE', 'DELETE']);
      assert.deepEqual(fixture.writes[0].body, { tenant_external_id: 'fixture', name: '研发出口组', members: [{ label: '主出口', proxy_url: privateProxySecret }] });
      assert.deepEqual(fixture.writes[1].body, { tenant_external_id: 'fixture', name: '共享出口组', expected_version: 1, members: [{ id: 'raw-member-id-0', label: '主出口' }] });
      assert.deepEqual(fixture.writes[2].body, { tenant_external_id: 'fixture', expected_binding_version: 0, expected_credential_generation: 7, expected_updated_at: 100, group_id: 'raw-group-id', expected_group_version: 2, initial_member_id: 'raw-member-id-0' });
      assert.deepEqual(fixture.writes[3].body, { tenant_external_id: 'fixture', expected_binding_version: 1, expected_credential_generation: 8, expected_updated_at: 101, expected_group_version: 2, single_proxy_member_id: 'raw-member-id-0' });
      assert.deepEqual(fixture.writes[4].body, { tenant_external_id: 'fixture', expected_version: 2 });
      assert.ok(fixture.policies.every(Boolean));
      await page.getByRole('dialog').getByRole('button', { name: '关闭', exact: true }).click();
      assert.equal(await page.locator('.transport-proxy-workspace').count(), 0);
      assert.equal(await page.locator('.provider-directory-row').isVisible(), true);
      await page.waitForFunction(() => document.activeElement?.textContent === '管理代理组');
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
      await page.getByRole('textbox', { name: /^出口名称\s*\*?$/ }).fill('修正后的出口');
      await page.getByRole('button', { name: '保存代理组', exact: true }).click();
      await page.getByRole('button', { name: '编辑 研发出口组', exact: true }).click();
      await page.getByRole('textbox', { name: /^代理组名称\s*\*?$/ }).fill('本地修改');
      await page.evaluate(() => { window.proxyGroupFixture.groups[0].version = 3; window.proxyGroupFixture.groups[0].name = '他人修改'; });
      await page.getByRole('button', { name: '保存代理组', exact: true }).click();
      await page.getByText(/代理组已被其他操作更新/).waitFor();
      assert.equal(await page.getByRole('button', { name: '保存代理组', exact: true }).isEnabled(), false);
      await page.getByRole('button', { name: '刷新配置', exact: true }).click(); await confirm(page);
      await page.getByRole('button', { name: '编辑 他人修改', exact: true }).click();
      await page.getByRole('textbox', { name: /^代理组名称\s*\*?$/ }).fill('确认后的修改');
      await page.getByRole('button', { name: '保存代理组', exact: true }).click();
      await page.getByRole('button', { name: '编辑 确认后的修改', exact: true }).waitFor();
      assert.equal(await page.evaluate(() => window.proxyGroupFixture.writes.at(-1)?.body.expected_version), 3);
      await page.getByLabel('账号', { exact: true }).selectOption({ label: '研发订阅' });
      await page.getByLabel('目标代理组', { exact: true }).selectOption({ label: '确认后的修改' });
      await page.getByLabel('起始出口', { exact: true }).selectOption({ label: '修正后的出口' });
      await page.evaluate(() => { window.proxyGroupFixture.binding.credential_generation = 20; });
      await page.getByRole('button', { name: '保存账号绑定', exact: true }).click();
      await page.getByText('账号的连接设置已变化。请刷新配置后重新选择。', { exact: true }).waitFor();
      assert.equal(await page.getByRole('button', { name: '保存账号绑定', exact: true }).isEnabled(), false);
      await page.getByRole('button', { name: '刷新配置', exact: true }).click();
      await page.getByLabel('目标代理组', { exact: true }).selectOption({ label: '确认后的修改' });
      await page.getByLabel('起始出口', { exact: true }).selectOption({ label: '修正后的出口' });
      await page.getByRole('button', { name: '保存账号绑定', exact: true }).click();
      await page.getByText('账号代理组已保存，正在应用新的连接设置。', { exact: true }).waitFor();
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
      await page.getByText(/保存结果尚未确认，关闭页面也不会取消保存/).waitFor();
      assert.equal(await page.getByRole('button', { name: '保存代理组', exact: true }).isEnabled(), false);
      await page.evaluate(() => window.proxyGroupFixture.release?.());
      assert.equal(await page.getByText(/代理组配置已保存/).count(), 0);
      assert.equal(await page.evaluate(() => window.proxyGroupFixture.writes.length), 1);
      await page.getByRole('button', { name: '刷新配置', exact: true }).click(); await confirm(page);
      await page.getByText('配置已刷新。请核对上次修改是否已保存，再决定是否重新提交。', { exact: true }).waitFor();
      await page.getByRole('button', { name: '编辑 研发出口组', exact: true }).click();
      await page.getByRole('textbox', { name: /^代理组名称\s*\*?$/ }).fill('等待中修改');
      await page.evaluate(() => { window.proxyGroupFixture.holdNext = true; window.proxyGroupFixture.release = undefined; });
      await page.getByRole('button', { name: '保存代理组', exact: true }).click();
      await page.waitForFunction(() => Boolean(window.proxyGroupFixture.release));
      await page.getByRole('dialog').getByRole('button', { name: '关闭', exact: true }).click();
      await page.getByText(/关闭不会取消保存/).waitFor(); await confirm(page);
      assert.equal(await page.locator('.provider-directory-row').isVisible(), true);
      await page.evaluate(() => window.proxyGroupFixture.release?.());
      await openManager(page);
      await page.getByRole('button', { name: '编辑 等待中修改', exact: true }).waitFor();
      assert.equal(await page.evaluate(() => window.proxyGroupFixture.writes.length), 2);
      await assertSecretsAbsent(page); await page.close();
    });

    await context.test('denied permission disables entry with explanation, not a failing management dialog', async () => {
      const page = await prepare(false);
      await page.getByText('你没有管理代理组的权限，请联系管理员开通。', { exact: true }).waitFor();
      assert.equal(await page.getByRole('button', { name: '管理代理组', exact: true }).isEnabled(), false);
      assert.equal(await page.locator('.transport-proxy-workspace').count(), 0);
      assert.doesNotMatch(await page.content(), /privateProxySecret/);
      assert.equal(await page.evaluate(() => window.proxyGroupFixture.writes.length), 0);
      assert.ok(await page.evaluate(() => window.proxyGroupFixture.accessReads >= 2));
      await page.evaluate(() => { window.proxyGroupFixture.accessFailure = true; });
      await page.getByRole('button', { name: '重试', exact: true }).click();
      await page.getByText('暂时无法打开代理组，请重试。', { exact: true }).waitFor();
      assert.equal(await page.getByText('你没有管理代理组的权限，请联系管理员开通。', { exact: true }).count(), 0);
      assert.equal(await page.getByRole('button', { name: '管理代理组', exact: true }).isEnabled(), false);
      assert.doesNotMatch(await page.content(), /privateProxySecret|providers:write/);
      await page.evaluate(() => { window.proxyGroupFixture.accessFailure = false; });
      await page.getByRole('button', { name: '重试', exact: true }).click();
      await page.getByText('你没有管理代理组的权限，请联系管理员开通。', { exact: true }).waitFor();
      assert.equal(await page.evaluate(() => window.proxyGroupFixture.reads), 0);
      await page.close();
    });
    assert.doesNotMatch(errors.join('\n'), /privateProxySecret|fixture-user/);
    assert.deepEqual(errors, []);
  } finally { await browser.close(); await server.close(); }
});
