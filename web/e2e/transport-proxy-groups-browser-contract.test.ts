import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium, type Page } from 'playwright';
import { createIsolatedFixtureServer } from './support/isolated-vite-server.js';
import { operatorFixturePlugin } from './support/navigation-fixture-server.js';
import { captureNavigationIdentity, seedNavigationIdentity } from './support/navigation-identity-artifacts.js';
import type { TransportProxyBinding, TransportProxyGroup } from '../src/operator/transportProxyGroups.js';
import { transportProxyGroupCopy } from '../src/operator/transportProxyGroupCopy.js';
import type { NetworkProxy, NetworkProxySelection } from '../src/operator/NetworkProxies.js';
import { networkProxyCopy } from '../src/operator/NetworkProxyCopy.js';
import { concurrencySnapshot, csilProxy, legacySelection, networkProxyStrategies, proxyObservations } from './fixtures/transport-proxy-entities.js';

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
  holdNextRead?: boolean;
  releaseRead?: () => void;
  readFailure?: boolean;
  policies: boolean[];
  proxies: NetworkProxy[];
  selection: NetworkProxySelection;
}

declare global {
  interface Window { proxyGroupFixture: ProxyGroupFixture; releaseNavigationProxyAccess?: () => void }
}

const privateProxySecret = 'socks5h://fixture-user:privateProxySecret@10.20.30.40:1080';

function fieldLabel(label: string): RegExp {
  return new RegExp(`^${label.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}\\s*\\*?$`);
}

async function installFixture(page: Page, allowed = true, locale = 'zh-CN', tab: 'proxies' | 'groups' | 'policies' = 'groups') {
  const copy = transportProxyGroupCopy(locale);
  const networkCopy = networkProxyCopy(locale);
  await page.evaluate(({ allowed, secret, proxy, selection, strategies, concurrency, observations }) => {
    const previousFetch = window.fetch;
    const state: ProxyGroupFixture = window.proxyGroupFixture = {
      allowed, groups: [], reads: 0, accessReads: 0, writes: [], holdNext: false, policies: [],
      proxies: [proxy], selection,
      binding: {
        account_id: 'account-native', tenant_external_id: 'fixture', binding_version: 0,
        group_id: null, group_version: null, initial_member_id: null, credential_generation: 7, updated_at: 100,
        runtime: { scope: 'this_process', configuration_state: 'unbound', selected_member_id: null, observed_at: 100 },
      },
    };
    window.fetch = async (input, init) => {
      const url = new URL(typeof input === 'string' ? input : input instanceof URL ? input.href : input.url, location.origin);
      const method = init?.method ?? 'GET';
      const entityApi = url.pathname === '/internal/v1/transport-proxies' || url.pathname.startsWith('/internal/v1/transport-proxies/')
        || url.pathname === '/internal/v1/transport-proxy-strategies' || url.pathname === '/internal/v1/upstream-concurrency'
        || url.pathname.endsWith('/transport-proxy-selection');
      if (entityApi) {
        state.policies.push(init?.cache === 'no-store' && init?.credentials === 'omit' && init?.referrerPolicy === 'no-referrer');
        if (!state.allowed) return new Response(JSON.stringify({ error: { code: 'forbidden', message: secret } }), { status: 403 });
        if (method === 'GET') {
          if (url.searchParams.get('tenant_external_id') !== 'fixture') throw new Error('missing entity read tenant');
          state.reads += 1;
          if (url.pathname === '/internal/v1/transport-proxies') {
            return new Response(JSON.stringify({ items: state.proxies.map(proxy => {
              const item: NetworkProxy = { ...proxy };
              delete item.group_ids; delete item.bound_account_count;
              return item;
            }) }));
          }
          if (url.pathname.endsWith('/observations')) {
            const id = url.pathname.split('/')[4];
            const proxy = state.proxies.find(item => item.id === id);
            return proxy ? new Response(JSON.stringify({ ...observations, proxy_id: proxy.id, proxy_version: proxy.version, active_health: proxy.active_health })) : new Response(null, { status: 404 });
          }
          if (url.pathname === '/internal/v1/transport-proxy-strategies') return new Response(JSON.stringify({ items: strategies }));
          if (url.pathname === '/internal/v1/upstream-concurrency') return new Response(JSON.stringify(concurrency));
          if (url.pathname.endsWith('/transport-proxy-selection')) return new Response(JSON.stringify(state.selection));
        }
        const body = JSON.parse(String(init?.body ?? '{}'));
        state.writes.push({ path: `${url.pathname}${url.search}`, method, body });
        if (body.tenant_external_id !== 'fixture') throw new Error('missing entity write tenant');
        if (url.pathname.endsWith('/probe')) return new Response(JSON.stringify({ queued: true }), { status: 202 });
        if (url.pathname.endsWith('/transport-proxy-selection')) {
          if (body.expected_selection_version !== state.selection.selection_version || body.expected_binding_version !== state.selection.binding_version
            || body.expected_credential_generation !== state.selection.credential_generation || body.expected_updated_at !== state.selection.account_updated_at) {
            return new Response(JSON.stringify({ error: { code: 'network_proxy_selection_version_conflict' } }), { status: 409 });
          }
          state.selection = {
            ...state.selection, selection_version: state.selection.selection_version + 1, binding_version: state.selection.binding_version + 1,
            credential_generation: state.selection.credential_generation + 1, account_updated_at: state.selection.account_updated_at + 1,
            configuration_revision: state.selection.selection_version + 1,
            desired: {
              mode: body.mode, proxy_id: body.proxy_id ?? null, group_id: body.group_id ?? null,
              plugin_id: body.plugin_id ?? null, plugin_version: body.plugin_version ?? null, config: body.config,
            },
            applied: { pending: true, scope: 'this_process' },
          };
          return new Response(JSON.stringify(state.selection), { status: 202 });
        }
        if (method === 'DELETE') {
          state.proxies = state.proxies.filter(proxy => proxy.id !== url.pathname.split('/').at(-1));
          return new Response(null, { status: 204 });
        }
        const id = method === 'POST' ? '10000000-0000-4000-8000-000000000002' : url.pathname.split('/').at(-1);
        const old = state.proxies.find(proxy => proxy.id === id);
        if (method === 'PUT' && (!old || body.expected_version !== old.version)) {
          return new Response(JSON.stringify({ error: { code: 'network_proxy_version_conflict' } }), { status: 409 });
        }
        const parsed = body.proxy_url ? new URL(body.proxy_url) : undefined;
        const saved: NetworkProxy = {
          id: id!, tenant_external_id: 'fixture', name: body.name, version: (old?.version ?? 0) + 1,
          address: {
            scheme: parsed?.protocol.slice(0, -1) ?? old?.address.scheme ?? 'socks5h',
            host: parsed?.hostname ?? old?.address.host ?? '10.20.30.40', port: parsed ? Number(parsed.port) : old?.address.port ?? 1080,
            remote_dns: true, has_auth: parsed ? Boolean(parsed.username || parsed.password) : old?.address.has_auth ?? false,
          },
          probe: body.probe, active_health: { status: 'unknown', checked_at: null, latency_ms: null, scope: 'process' },
          group_ids: [], bound_account_count: 0, created_at: old?.created_at ?? 1_793_510_000_000, updated_at: 1_793_510_600_000,
        };
        state.proxies = [...state.proxies.filter(proxy => proxy.id !== id), saved];
        return new Response(JSON.stringify(saved), { status: method === 'POST' ? 201 : 200 });
      }
      if (!url.pathname.includes('transport-proxy-group')) return previousFetch(input, init);
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
        if (state.readFailure) return new Response(JSON.stringify({ error: { message: secret } }), { status: 503 });
        const response = new Response(JSON.stringify(url.pathname.endsWith('/transport-proxy-group') ? state.binding : { items: state.groups }));
        if (state.holdNextRead) {
          state.holdNextRead = false;
          return new Promise<Response>(resolve => { state.releaseRead = () => resolve(response); });
        }
        return response;
      }
      if (method === 'POST' && url.pathname.endsWith('/probe')) {
        state.writes.push({ path: `${url.pathname}${url.search}`, method, body: {} });
        return new Response(null, { status: 202 });
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
          probe_url: body.probe_url, probe_attempts: body.probe_attempts,
          members: body.members.map((member: { id?: string; label: string; proxy_url?: string }, index: number) => {
            const saved = old?.members.find(saved => saved.id === member.id);
            const address = member.proxy_url ? new URL(member.proxy_url) : undefined;
            return {
              id: member.id ?? (old ? `raw-member-id-${old.version + 1}-${index}` : `raw-member-id-${index}`),
              label: member.label, scheme: 'socks5h', host: address?.hostname ?? saved?.host ?? '10.20.30.40',
              port: address ? Number(address.port) : saved?.port ?? 1080, remote_dns: true,
              has_auth: address ? Boolean(address.username || address.password) : saved?.has_auth ?? false,
            };
          }),
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
  }, { allowed, secret: privateProxySecret, proxy: csilProxy, selection: legacySelection,
    strategies: networkProxyStrategies, concurrency: concurrencySnapshot, observations: proxyObservations(csilProxy) });
  const probe = await page.evaluate(async () => {
    const response = await window.fetch('/internal/v1/transport-proxy-groups/access', {
      cache: 'no-store', credentials: 'omit', referrerPolicy: 'no-referrer',
    });
    const body = await response.json();
    return { status: response.status, canManage: body.can_manage };
  });
  assert.equal(probe.status, 200, 'fixture self-capability response must be ready before UI interaction');
  assert.equal(probe.canManage, allowed);
  await page.locator('.proxy-groups-page').getByRole('button', { name: copy.retry, exact: true }).click();
  if (allowed) {
    const tabName = tab === 'groups' ? copy.title : tab === 'policies' ? networkCopy.policiesTab : networkCopy.proxiesTab;
    await page.getByRole('tab', { name: tabName, exact: true }).click();
    if (tab === 'policies') await page.getByLabel(fieldLabel(networkCopy.account)).waitFor();
    else await page.getByRole('button', { name: tab === 'groups' ? copy.create : networkCopy.create, exact: true }).waitFor();
  }
  else await page.getByText(copy.denied, { exact: true }).waitFor();
  assert.equal(await page.getByText('已具备代理组管理权限。', { exact: true }).count(), 0);
  if (!allowed) assert.equal(await page.evaluate(() => window.proxyGroupFixture.reads), 0, 'denied capability checks must not read group or binding data');
}

async function openManager(page: Page) {
  await page.getByRole('link', { name: '网络代理', exact: true }).click();
  await page.getByRole('tab', { name: '代理组', exact: true }).click();
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
  for (const value of Object.values(serialized)) {
    const leak = value.search(/privateProxySecret|fixture-user/);
    assert.equal(leak, -1, `secret serialized near: ${JSON.stringify(value.slice(Math.max(0, leak - 200), leak + 100))}`);
  }
  assert.doesNotMatch(await page.locator('.transport-proxy-workspace').innerText(), /raw-group-id|raw-member-id|account-native/);
}

async function assertWorkspaceFocus(page: Page) {
  assert.deepEqual(await page.locator('.transport-proxy-workspace').evaluate(workspace => ({
    containsFocus: workspace.contains(document.activeElement),
    hidden: Boolean(workspace.closest('[aria-hidden="true"], [inert]')),
  })), { containsFocus: true, hidden: false }, 'the open workspace keeps focus and stays accessible after its initiating control is removed');
}

async function assertAccountDirectoryReturn(page: Page) {
  const directory = page.locator('.provider-directory-row');
  await directory.waitFor();
  assert.equal(await directory.isVisible(), true, 'returning from the proxy page loads the account directory');
  assert.equal(await page.locator('.transport-proxy-workspace').count(), 0);
  assert.equal(await page.getByRole('dialog').count(), 0);
  await page.waitForFunction(() => document.activeElement?.id === 'app-main-content');
  assert.equal(await page.locator('#app-main-content').evaluate(main => main === document.activeElement), true, 'page navigation restores focus to the main content, not a removed dialog trigger');
  await page.keyboard.press('Tab');
  assert.equal(await page.locator('#app-main-content').evaluate(main => {
    const active = document.activeElement;
    return active instanceof HTMLElement && active !== main && main.contains(active)
      && !active.closest('[hidden], [inert], [aria-hidden="true"]');
  }), true, 'keyboard navigation can enter the returned account page');
}

test('transport proxy groups: CRUD, policies, validation, CAS, secrets, permissions and bounded exit', { timeout: 120_000 }, async context => {
  const root = fileURLToPath(new URL('..', import.meta.url));
  const server = await createIsolatedFixtureServer({ root, configFile: false, logLevel: 'silent', plugins: [operatorFixturePlugin('/e2e/fixtures/transport-proxy-groups.html')], server: { host: '127.0.0.1', port: 0 } });
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
  async function prepare(allowed = true, locale = 'zh-CN', tab: 'proxies' | 'groups' | 'policies' = 'groups') {
    const page = await browser.newPage();
    activePage = page;
    page.on('pageerror', error => errors.push(error.message));
    page.on('console', message => { if (message.type() === 'error') errors.push(message.text()); });
    await page.route('**/*', route => {
      const url = new URL(route.request().url());
      return url.origin === origin && !url.pathname.startsWith('/internal/') ? route.continue() : route.abort();
    });
    await seedNavigationIdentity(page, locale);
    await page.goto(`${origin}/operator?view=proxy-groups`);
    await page.getByText(transportProxyGroupCopy(locale).denied, { exact: true }).waitFor();
    await installFixture(page, allowed, locale, tab);
    return page;
  }
  try {
    await context.test('desktop and narrow bilingual screenshots cover all applicable proxy workspace states', async () => {
      for (const locale of ['zh-CN', 'en']) {
        const copy = transportProxyGroupCopy(locale);
        const page = await prepare(true, locale);
        await page.getByText(copy.empty, { exact: true }).waitFor();
        await captureNavigationIdentity(page, 'proxy-groups', 'empty', locale);
        await page.evaluate(() => { window.proxyGroupFixture.groups = [{
          id: 'raw-group-id', tenant_external_id: 'fixture', name: 'Research exits', version: 1, bound_account_count: 0,
          members: [{ id: 'raw-member-id-0', label: 'Primary exit', scheme: 'socks5h', host: '10.20.30.40', port: 1080, remote_dns: true, has_auth: true }],
        }]; });
        await page.getByRole('button', { name: copy.refresh, exact: true }).click();
        await page.getByRole('button', { name: copy.edit('Research exits'), exact: true }).waitFor();
        await captureNavigationIdentity(page, 'proxy-groups', 'ready', locale);
        const heading = page.getByRole('heading', { name: copy.pageTitle, exact: true });
        const back = page.getByRole('button', { name: copy.backToAccounts, exact: true });
        const [headingBox, backBox] = await Promise.all([heading.boundingBox(), back.boundingBox()]);
        assert.ok(headingBox && backBox && backBox.x < headingBox.x, 'back navigation stays at the upper left');
        assert.equal(await page.locator('.proxy-groups-page').evaluate(element => getComputedStyle(element).borderTopWidth), '0px');
        await page.getByRole('button', { name: copy.edit('Research exits'), exact: true }).click();
        assert.equal(await page.getByLabel(copy.replaceAddress, { exact: true }).getAttribute('placeholder'), 'socks5h://10.20.30.40:1080');
        await captureNavigationIdentity(page, 'proxy-groups', 'edit-address', locale);
        await page.getByRole('button', { name: copy.cancelEdit, exact: true }).click();
        await page.locator('.app-confirm-dialog button').last().click();
        await page.getByRole('button', { name: copy.create, exact: true }).waitFor({ state: 'visible' });
        await page.evaluate(() => { window.proxyGroupFixture.holdNextRead = true; });
        await page.getByRole('button', { name: copy.refresh, exact: true }).click();
        await page.waitForFunction(() => Boolean(window.proxyGroupFixture.releaseRead));
        const retainedGroup = page.getByRole('button', { name: copy.edit('Research exits'), exact: true });
        assert.equal(await retainedGroup.isVisible(), true, 'refresh keeps the previous real group visible');
        assert.equal(await retainedGroup.isDisabled(), true, 'refresh prevents stale group writes');
        await captureNavigationIdentity(page, 'proxy-groups', 'background-refresh', locale);
        await page.evaluate(() => window.proxyGroupFixture.releaseRead?.());
        await page.getByRole('button', { name: copy.refresh, exact: true }).waitFor();
        assert.equal(await retainedGroup.isEnabled(), true);
        await page.evaluate(() => { window.proxyGroupFixture.readFailure = true; });
        await page.getByRole('button', { name: copy.refresh, exact: true }).click();
        await page.locator('.transport-proxy-workspace [role="alert"]').waitFor();
        assert.equal(await page.locator('.transport-proxy-workspace [role="alert"]').innerText(), copy.errors.load);
        assert.equal(await retainedGroup.isVisible(), true, 'a failed refresh must not invent an empty collection');
        assert.doesNotMatch(await page.content(), /privateProxySecret|fixture-user/);
        await captureNavigationIdentity(page, 'proxy-groups', 'error', locale);
        await page.close();
        const denied = await prepare(false, locale);
        assert.equal(await denied.locator('.transport-proxy-workspace').count(), 0);
        assert.equal(await denied.evaluate(() => window.proxyGroupFixture.reads), 0);
        await captureNavigationIdentity(denied, 'proxy-groups', 'permission-failure', locale);
        await denied.close();
        const slow = await browser.newPage();
        await seedNavigationIdentity(slow, locale);
        await slow.goto(`${origin}/operator?view=proxy-groups&access=slow`);
        await slow.getByLabel(copy.checking, { exact: true }).waitFor();
        await slow.waitForFunction(() => Boolean(window.releaseNavigationProxyAccess));
        assert.equal(await slow.locator('.transport-proxy-workspace').count(), 0);
        await captureNavigationIdentity(slow, 'proxy-groups', 'initial-loading', locale);
        await slow.evaluate(() => window.releaseNavigationProxyAccess?.());
        await slow.getByText(copy.denied, { exact: true }).waitFor();
        await slow.close();
      }
    });
    await context.test('one standalone workspace has sidebar navigation and guarded drafts without a management dialog', async () => {
      const page = await prepare();
      const workspace = page.locator('.transport-proxy-workspace');
      await workspace.getByRole('button', { name: '新建代理组', exact: true }).waitFor();
      assert.equal(await workspace.count(), 1);
      assert.equal(await page.getByRole('button', { name: '管理代理组', exact: true }).count(), 0);
      assert.equal(await page.getByRole('dialog').count(), 0);
      const sidebar = page.getByRole('link', { name: '网络代理', exact: true });
      assert.equal(await sidebar.getAttribute('aria-current'), 'page');
      assert.match(await sidebar.getAttribute('href') ?? '', /view=proxy-groups/);
      assert.equal(await workspace.getByLabel(fieldLabel('账号')).count(), 0, 'account settings have one home in the policy workspace');
      for (const width of [1440, 390]) {
        await page.setViewportSize({ width, height: 1000 });
        const bounds = await workspace.boundingBox();
        assert.ok(bounds && bounds.x >= -1 && bounds.x + bounds.width <= width + 1);
      }
      await captureNavigationIdentity(page, 'proxy-groups', 'group-management', 'zh-CN');
      await page.setViewportSize({ width: 1440, height: 1000 });
      await page.getByRole('button', { name: '返回上游账号', exact: true }).click();
      await page.locator('.provider-directory-row').waitFor();
      await openManager(page);
      await workspace.getByRole('button', { name: '新建代理组', exact: true }).waitFor();
      await enterDraft(page);
      await page.goBack();
      await page.getByRole('dialog').waitFor();
      await page.keyboard.press('Escape');
      await page.waitForFunction(() => new URL(location.href).searchParams.get('view') === 'proxy-groups');
      assert.equal(await workspace.getByRole('textbox', { name: /^代理组名称\s*\*?$/ }).inputValue(), '研发出口组');
      await page.getByRole('button', { name: '返回上游账号', exact: true }).click();
      await page.getByRole('dialog').waitFor();
      await page.keyboard.press('Escape');
      assert.equal(await workspace.getByRole('textbox', { name: /^代理组名称\s*\*?$/ }).inputValue(), '研发出口组');
      assert.equal(await page.getByRole('dialog').count(), 0);
      assert.equal(await page.evaluate(() => window.proxyGroupFixture.writes.length), 0);
      await page.getByRole('button', { name: '返回上游账号', exact: true }).click();
      await confirm(page);
      await page.locator('.provider-directory-row').waitFor();
      assert.equal(await workspace.count(), 0);
      await page.close();
    });

    await context.test('permission loss clears private draft fields and does not block leaving with a draft guard', async () => {
      const page = await prepare();
      await enterDraft(page);
      await page.evaluate(() => { window.proxyGroupFixture.failure = { status: 403, code: 'forbidden' }; });
      await page.getByRole('button', { name: '保存代理组', exact: true }).click();
      await page.getByText(transportProxyGroupCopy('zh-CN').errors.denied, { exact: true }).waitFor();
      assert.equal(await page.getByLabel('私网代理地址（必填）', { exact: true }).count(), 0);
      await assertSecretsAbsent(page);
      await page.getByRole('button', { name: '返回上游账号', exact: true }).click();
      await page.locator('.provider-directory-row').waitFor();
      assert.equal(await page.getByRole('dialog').count(), 0);
      await page.close();
    });

    await context.test('account choice deep-links to the unified policy workspace and restores scoped policy after refresh', async () => {
      const page = await prepare();
      await page.getByRole('button', { name: '返回上游账号', exact: true }).click();
      const row = page.locator('.provider-directory-row');
      await row.waitFor();
      assert.equal(await page.getByRole('button', { name: '管理代理组', exact: true }).count(), 0);
      await row.locator('[data-manage-account-trigger]').click();
      await page.locator('.provider-detail-workspace .upstream-connection').getByRole('button', { name: '选择代理组', exact: true }).click();
      const workspace = page.locator('.transport-proxy-workspace');
      await workspace.getByText(/沿用旧版绑定，尚未设置新策略/).waitFor();
      assert.equal(await workspace.count(), 1);
      assert.equal(await workspace.getByLabel(fieldLabel('账号')).inputValue(), 'account-native');
      assert.match(page.url(), /view=proxy-groups/);
      const url = new URL(page.url());
      assert.equal(url.searchParams.get('account'), 'account-native');
      assert.equal(url.searchParams.get('tenant'), 'fixture');
      assert.equal(await page.getByRole('dialog').count(), 0);
      assert.equal(await page.evaluate(() => window.proxyGroupFixture.writes.length), 0);
      await page.reload();
      await page.getByText(transportProxyGroupCopy('zh-CN').denied, { exact: true }).waitFor();
      await installFixture(page, true, 'zh-CN', 'policies');
      await workspace.getByText(/沿用旧版绑定，尚未设置新策略/).waitFor();
      assert.equal(await workspace.getByLabel(fieldLabel('账号')).inputValue(), 'account-native');
      await page.goBack();
      await row.waitFor();
      assert.equal(await workspace.count(), 0);
      await page.goForward();
      await workspace.getByText(/沿用旧版绑定，尚未设置新策略/).waitFor();
      assert.equal(await workspace.getByLabel(fieldLabel('账号')).inputValue(), 'account-native');
      await page.close();
    });

    await context.test('English group workspace explains the feature instead of reporting permission status', async () => {
      const page = await prepare(true, 'en');
      const workspace = page.locator('.transport-proxy-workspace');
      await workspace.getByRole('button', { name: 'Create proxy group', exact: true }).waitFor();
      assert.match(await workspace.innerText(), /Give an account backup network exits/);
      assert.doesNotMatch(await workspace.innerText(), /代理组|私网代理地址|已具备|permission granted|worklog|raw-group-id/);
      await workspace.getByRole('button', { name: 'Create proxy group', exact: true }).click();
      assert.equal(await workspace.getByLabel('Proxy group name', { exact: false }).count(), 1);
      assert.equal(await workspace.getByLabel('Private proxy address (required)', { exact: true }).getAttribute('type'), 'text');
      await page.setViewportSize({ width: 390, height: 1000 });
      const bounds = await workspace.boundingBox();
      assert.ok(bounds && bounds.x >= -1 && bounds.x + bounds.width <= 391);
      assert.equal(await page.evaluate(() => window.proxyGroupFixture.writes.length), 0);
      await captureNavigationIdentity(page, 'proxy-groups', 'create-draft', 'en');
      await page.close();
    });

    await context.test('names, private secret handling and group CRUD CAS payloads', async () => {
      const page = await prepare();
      await openManager(page); await enterDraft(page);
      assert.equal(await page.getByLabel('私网代理地址（必填）', { exact: true }).getAttribute('type'), 'text');
      await assertSecretsAbsent(page);
      await page.getByRole('button', { name: '保存代理组', exact: true }).click();
      await page.getByText(/代理组配置已保存/).waitFor();
      await assertWorkspaceFocus(page);
      await page.getByRole('button', { name: '编辑 研发出口组', exact: true }).click();
      assert.equal(await page.getByLabel('替换代理地址（留空保留原值）', { exact: true }).inputValue(), '');
      await page.getByRole('textbox', { name: /^代理组名称\s*\*?$/ }).fill('共享出口组');
      await page.getByRole('button', { name: '保存代理组', exact: true }).click();
      await page.getByRole('button', { name: '编辑 共享出口组', exact: true }).waitFor();
      await assertWorkspaceFocus(page);
      await assertSecretsAbsent(page);
      await page.locator('.transport-proxy-workspace').getByRole('button', { name: '删除', exact: true }).click(); await confirm(page);
      await page.getByText('代理组已删除。', { exact: true }).waitFor();
      await assertWorkspaceFocus(page);
      const fixture = await page.evaluate(() => window.proxyGroupFixture);
      assert.deepEqual(fixture.writes.map(write => write.method), ['POST', 'PUT', 'DELETE']);
      assert.deepEqual(fixture.writes[0].body, { tenant_external_id: 'fixture', name: '研发出口组', probe_url: null, probe_attempts: 2, members: [{ label: '主出口', proxy_url: privateProxySecret }] });
      assert.deepEqual(fixture.writes[1].body, { tenant_external_id: 'fixture', name: '共享出口组', expected_version: 1, probe_url: null, probe_attempts: 2, members: [{ id: 'raw-member-id-0', label: '主出口' }] });
      assert.deepEqual(fixture.writes[2].body, { tenant_external_id: 'fixture', expected_version: 2 });
      assert.ok(fixture.policies.every(Boolean));
      await page.getByRole('button', { name: '返回上游账号', exact: true }).click();
      assert.equal(await page.locator('.transport-proxy-workspace').count(), 0);
      await assertAccountDirectoryReturn(page);
      await page.close();
    });

    await context.test('bound exits can be viewed, edited, removed and fully replaced in one save while probes remain available', async () => {
      const page = await prepare();
      const copy = transportProxyGroupCopy('zh-CN');
      await page.evaluate(() => {
        window.proxyGroupFixture.groups = [{
          id: 'raw-group-id', tenant_external_id: 'fixture', name: 'CSIL', version: 1, bound_account_count: 1,
          probe_url: 'https://example.com/', probe_attempts: 3, last_probe_status: 'healthy', last_probe_checked_at: 100,
          members: [
            { id: 'raw-member-id-0', label: '主出口', scheme: 'socks5h', host: '100.64.0.16', port: 1080, remote_dns: true, has_auth: false },
            { id: 'raw-member-id-1', label: '备用出口', scheme: 'socks5h', host: '100.64.0.17', port: 1080, remote_dns: true, has_auth: false },
          ],
        }];
      });
      await page.getByRole('button', { name: copy.refresh, exact: true }).click();
      const edit = page.getByRole('button', { name: copy.edit('CSIL'), exact: true });
      await edit.click();
      const address = page.getByLabel(copy.address, { exact: true });
      assert.equal(await address.first().inputValue(), 'socks5h://100.64.0.16:1080');
      assert.equal(await address.first().getAttribute('type'), 'text');
      assert.equal(await page.getByText(/上次独立探测：可达/).count(), 1);
      await captureNavigationIdentity(page, 'proxy-groups', 'bound-edit', 'zh-CN');
      await page.getByRole('button', { name: copy.probeNow, exact: true }).click();
      await page.getByText(copy.probeQueued, { exact: true }).waitFor();
      const save = page.getByRole('button', { name: copy.save, exact: true });
      await save.click();
      await edit.click();
      let writes = await page.evaluate(() => window.proxyGroupFixture.writes);
      assert.match(writes[0].path, /\/probe\?tenant_external_id=fixture$/);
      assert.deepEqual(writes[1].body.members, [{ id: 'raw-member-id-0', label: '主出口' }, { id: 'raw-member-id-1', label: '备用出口' }]);
      await page.getByRole('button', { name: copy.removeMember, exact: true }).first().click();
      assert.equal(await page.getByLabel('移除当前出口后使用', { exact: false }).count(), 0);
      await save.click();
      await edit.click();
      writes = await page.evaluate(() => window.proxyGroupFixture.writes);
      assert.deepEqual(writes[2].body.members, [{ id: 'raw-member-id-1', label: '备用出口' }]);
      assert.equal(await address.inputValue(), 'socks5h://100.64.0.17:1080');
      await address.fill('socks5h://100.64.0.18:1080');
      await save.click();
      await edit.click();
      assert.equal(await address.inputValue(), 'socks5h://100.64.0.18:1080');
      await page.getByRole('button', { name: copy.addMember, exact: true }).click();
      await page.getByRole('textbox', { name: /^出口名称\s*\*?$/ }).last().fill('新出口');
      await page.getByLabel(copy.requiredAddress, { exact: true }).fill('socks5h://100.64.0.16:1080');
      await page.getByRole('button', { name: copy.removeMember, exact: true }).first().click();
      assert.equal(await save.isEnabled(), true);
      await save.click();
      await page.getByText(copy.saved, { exact: true }).waitFor();
      writes = await page.evaluate(() => window.proxyGroupFixture.writes);
      assert.equal(writes.length, 5, 'each edit submits exactly once');
      assert.deepEqual(writes[3].body.members, [{ id: 'raw-member-id-1', label: '备用出口', proxy_url: 'socks5h://100.64.0.18:1080' }]);
      assert.deepEqual(writes[4].body.members, [{ label: '新出口', proxy_url: 'socks5h://100.64.0.16:1080' }]);
      for (const write of writes.slice(1)) {
        assert.equal(Object.hasOwn(write.body, 'replacement_member_id'), false);
        assert.equal(write.body.probe_url, 'https://example.com/');
        assert.equal(write.body.probe_attempts, 3);
      }
      assert.equal(writes[4].body.expected_version, 4);
      await captureNavigationIdentity(page, 'proxy-groups', 'saved-replacement', 'zh-CN');
      await page.close();
    });

    await context.test('standalone network exits show real health, success rates, traffic and concurrency in both languages', async () => {
      for (const locale of ['zh-CN', 'en']) {
        const copy = networkProxyCopy(locale);
        const page = await prepare(true, locale, 'proxies');
        const workspace = page.locator('.network-proxy-workspace');
        await workspace.getByRole('button', { name: copy.edit('CSIL'), exact: true }).waitFor();
        assert.equal(await workspace.getByText('socks5h://100.64.0.16:1080', { exact: true }).count(), 1);
        if (locale === 'zh-CN') assert.equal(await workspace.getByText(/0个组引用/).count(), 0, 'missing list references are not rendered as zero');
        await captureNavigationIdentity(page, 'proxy-groups', 'network-exits-ready', locale);
        await workspace.getByRole('button', { name: copy.details, exact: true }).click();
        await workspace.getByText(new RegExp(`${copy.requestSuccessRate}: 90%`)).waitFor();
        const details = await workspace.locator('.network-proxy-item').innerText();
        assert.match(details, new RegExp(`${copy.transportSuccessRate}: 95%`));
        assert.match(details, /4,096 B/);
        assert.match(details, /16,384 B/);
        assert.match(details, new RegExp(`${copy.concurrency}: 1`));
        assert.match(details, new RegExp(`${copy.healthTitle}: ${copy.health.healthy}`));
        await captureNavigationIdentity(page, 'proxy-groups', 'network-exits-details', locale);
        await workspace.getByRole('button', { name: copy.edit('CSIL'), exact: true }).click();
        const address = workspace.locator('.proxy-address-input');
        assert.equal(await address.inputValue(), 'socks5h://100.64.0.16:1080');
        assert.equal(await address.getAttribute('type'), 'text');
        await captureNavigationIdentity(page, 'proxy-groups', 'network-exit-edit-address', locale);
        await page.close();
      }
    });

    await context.test('entity address edits and manual, automatic and plugin policies save and reload through existing APIs', async () => {
      const page = await prepare(true, 'zh-CN', 'proxies');
      const copy = networkProxyCopy('zh-CN');
      const workspace = page.locator('.network-proxy-workspace');
      await workspace.getByRole('button', { name: copy.edit('CSIL'), exact: true }).click();
      await workspace.getByRole('textbox', { name: /^出口名称\s*\*?$/ }).fill('CSIL 主出口');
      await workspace.getByRole('button', { name: copy.save, exact: true }).click();
      await workspace.getByText(copy.saved, { exact: true }).waitFor();
      let writes = await page.evaluate(() => window.proxyGroupFixture.writes);
      assert.equal(writes.length, 1);
      assert.equal(writes[0].method, 'PUT');
      assert.equal(Object.hasOwn(writes[0].body, 'proxy_url'), false, 'unchanged redacted address preserves stored authentication');
      assert.equal(writes[0].body.expected_version, 1);

      await workspace.getByRole('button', { name: copy.create, exact: true }).click();
      await workspace.getByRole('textbox', { name: /^出口名称\s*\*?$/ }).fill('研发备用');
      await workspace.locator('.proxy-address-input').fill(privateProxySecret);
      await workspace.getByRole('button', { name: copy.save, exact: true }).click();
      await workspace.getByRole('button', { name: copy.edit('研发备用'), exact: true }).waitFor();
      await workspace.getByRole('button', { name: copy.edit('研发备用'), exact: true }).click();
      const replacement = workspace.locator('.proxy-address-input');
      assert.equal(await replacement.inputValue(), 'socks5h://10.20.30.40:1080');
      await replacement.fill('socks5h://10.20.30.41:1080');
      await workspace.getByRole('button', { name: copy.save, exact: true }).click();
      await workspace.getByText(copy.saved, { exact: true }).waitFor();
      await page.waitForFunction(() => window.proxyGroupFixture.writes.length === 3);
      writes = await page.evaluate(() => window.proxyGroupFixture.writes);
      assert.equal(writes.length, 3);
      assert.equal(writes[1].method, 'POST');
      assert.equal(writes[1].body.proxy_url, privateProxySecret);
      assert.equal(writes[2].method, 'PUT');
      assert.equal(writes[2].body.proxy_url, 'socks5h://10.20.30.41:1080');
      await assertSecretsAbsent(page);

      await page.evaluate(() => { window.proxyGroupFixture.groups = [{
        id: 'raw-group-id', tenant_external_id: 'fixture', name: 'CSIL 代理组', version: 1, bound_account_count: 0,
        members: [{ id: 'raw-member-id-0', label: 'CSIL', scheme: 'socks5h', host: '100.64.0.16', port: 1080, remote_dns: true, has_auth: false }],
      }]; });
      await page.getByRole('tab', { name: copy.policiesTab, exact: true }).click();
      const policies = page.locator('.network-proxy-policy-workspace');
      await policies.getByLabel(fieldLabel(copy.account)).selectOption('account-native');
      await policies.getByText(new RegExp(copy.policyLegacy)).waitFor();
      await policies.getByLabel(fieldLabel(copy.targetProxy)).selectOption({ label: 'CSIL 主出口' });
      await policies.getByRole('button', { name: copy.savePolicy, exact: true }).click();
      await policies.getByText(copy.policySaved, { exact: true }).waitFor();
      await captureNavigationIdentity(page, 'proxy-groups', 'account-policy-manual', 'zh-CN');

      await policies.getByLabel(fieldLabel(copy.mode)).selectOption('auto');
      await policies.getByLabel(fieldLabel(copy.targetGroup)).selectOption({ label: 'CSIL 代理组' });
      await policies.getByRole('button', { name: copy.savePolicy, exact: true }).click();
      await policies.getByText(copy.policySaved, { exact: true }).waitFor();
      await policies.getByLabel(fieldLabel(copy.mode)).selectOption('plugin');
      await policies.getByLabel(fieldLabel(copy.strategy)).selectOption({ label: 'fixture-latency · 2' });
      await policies.getByLabel(fieldLabel(copy.targetGroup)).selectOption({ label: 'CSIL 代理组' });
      await policies.getByLabel(fieldLabel('minimum_samples')).fill('6');
      await policies.getByRole('button', { name: copy.savePolicy, exact: true }).click();
      await policies.getByText(copy.policySaved, { exact: true }).waitFor();
      await page.waitForFunction(() => window.proxyGroupFixture.writes.length === 6);
      writes = await page.evaluate(() => window.proxyGroupFixture.writes);
      const policyWrites = writes.slice(3);
      assert.deepEqual(policyWrites.map(write => write.body.mode), ['manual', 'auto', 'plugin']);
      assert.equal(policyWrites[0].body.proxy_id, csilProxy.id);
      assert.deepEqual(policyWrites[0].body.config, {});
      assert.equal(policyWrites[1].body.group_id, 'raw-group-id');
      assert.equal(policyWrites[1].body.expected_group_version, 1);
      assert.equal(policyWrites[1].body.config.minimum_samples, 8);
      assert.equal(policyWrites[2].body.plugin_id, 'fixture-latency');
      assert.equal(policyWrites[2].body.plugin_version, '2');
      assert.deepEqual(policyWrites[2].body.config, { minimum_samples: 6, prefer_healthy: true });
      assert.deepEqual(policyWrites.map(write => write.body.expected_selection_version), [0, 1, 2]);
      assert.ok((await page.evaluate(() => window.proxyGroupFixture.policies)).every(Boolean));

      await page.getByRole('tab', { name: '代理组', exact: true }).click();
      await page.getByRole('tab', { name: copy.policiesTab, exact: true }).click();
      await policies.getByLabel(fieldLabel(copy.account)).selectOption('account-native');
      await policies.getByLabel(fieldLabel(copy.mode)).waitFor();
      assert.equal(await policies.getByLabel(fieldLabel(copy.mode)).inputValue(), 'plugin');
      assert.equal(await policies.getByLabel(fieldLabel(copy.strategy)).inputValue(), 'fixture-latency\0' + '2');
      assert.equal(await policies.getByLabel(fieldLabel('minimum_samples')).inputValue(), '6');
      await page.evaluate(() => { window.proxyGroupFixture.selection.credential_generation = 20; });
      await policies.getByLabel(fieldLabel('minimum_samples')).fill('7');
      await policies.getByRole('button', { name: copy.savePolicy, exact: true }).click();
      await policies.getByText(copy.errors.selectionConflict, { exact: true }).waitFor();
      assert.equal(await policies.getByRole('button', { name: copy.savePolicy, exact: true }).isEnabled(), false);
      await policies.getByRole('button', { name: copy.refresh, exact: true }).click(); await confirm(page);
      await policies.getByLabel(fieldLabel('minimum_samples')).fill('7');
      await policies.getByRole('button', { name: copy.savePolicy, exact: true }).click();
      await page.waitForFunction(() => window.proxyGroupFixture.writes.length === 8);
      assert.equal(await page.evaluate(() => window.proxyGroupFixture.writes.at(-1)?.body.expected_credential_generation), 20);
      assert.equal(await page.evaluate(() => window.proxyGroupFixture.writes.at(-1)?.body.config.minimum_samples), 7);
      await page.close();
    });

    await context.test('400 preserves editable draft and group CAS requires a read before retry', async () => {
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
      await page.close();
    });

    await context.test('timeout is unknown, late response cannot publish success, pending write can exit', async () => {
      const page = await prepare(); await openManager(page); await enterDraft(page);
      await page.clock.install();
      await page.evaluate(() => { window.proxyGroupFixture.holdNext = true; });
      await page.getByRole('button', { name: '保存代理组', exact: true }).click();
      await page.waitForFunction(() => Boolean(window.proxyGroupFixture.release));
      await page.clock.runFor(15_001);
      await page.getByText(/保存结果尚未确认。关闭页面后保存仍会完成/).waitFor();
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
      await page.getByRole('button', { name: '返回上游账号', exact: true }).click();
      await page.getByText(/关闭页面后保存仍会完成/).waitFor(); await confirm(page);
      await assertAccountDirectoryReturn(page);
      await page.evaluate(() => window.proxyGroupFixture.release?.());
      await openManager(page);
      await page.getByRole('button', { name: '编辑 等待中修改', exact: true }).waitFor();
      assert.equal(await page.evaluate(() => window.proxyGroupFixture.writes.length), 2);
      await assertSecretsAbsent(page); await page.close();
    });

    await context.test('denied permission disables entry with explanation, not a failing management dialog', async () => {
      const page = await prepare(false);
      await page.getByText('你没有管理代理组的权限，请联系管理员开通。', { exact: true }).waitFor();
      assert.equal(await page.getByRole('button', { name: '管理代理组', exact: true }).count(), 0);
      assert.equal(await page.locator('.transport-proxy-workspace').count(), 0);
      assert.doesNotMatch(await page.content(), /privateProxySecret/);
      assert.equal(await page.evaluate(() => window.proxyGroupFixture.writes.length), 0);
      assert.ok(await page.evaluate(() => window.proxyGroupFixture.accessReads >= 2));
      await page.evaluate(() => { window.proxyGroupFixture.accessFailure = true; });
      await page.getByRole('button', { name: '重试', exact: true }).click();
      await page.getByText('暂时无法打开代理组，请重试。', { exact: true }).waitFor();
      assert.equal(await page.getByText('你没有管理代理组的权限，请联系管理员开通。', { exact: true }).count(), 0);
      assert.equal(await page.getByRole('button', { name: '新建代理组', exact: true }).count(), 0);
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
