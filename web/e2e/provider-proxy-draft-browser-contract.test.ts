import assert from 'node:assert/strict';
import { editProviderAccount, manageProviderAccount } from './support/provider-account-navigation.js';
import { mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer } from './support/isolated-vite-server.js';
import { providerConnectionCopy } from '../src/operator/providerConnectionCopy.js';

test('independent proxy save updates concurrency metadata without dropping the provider draft', { timeout: 60_000 }, async () => {
  const root = fileURLToPath(new URL('..', import.meta.url));
  const server = await createIsolatedFixtureServer({ root, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const origin = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    await page.clock.setFixedTime(new Date('2026-09-13T07:00:00Z'));
    const errors: string[] = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.route('**/*', route => {
      const url = new URL(route.request().url());
      return url.origin === origin && !url.pathname.startsWith('/internal/') ? route.continue() : route.abort();
    });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'zh-CN'));
    // All saves below are handled by an explicit in-memory fixture. No server
    // mutation, real proxy connection, OAuth, quota action or model request runs.
    await page.goto(`${origin}/e2e/fixtures/form-journey.html?workflows&proxy-workflow`);
    const row = page.locator('.provider-directory-row');
    const quotaDetails = page.locator('.provider-detail-workspace .upstream-quota');
    await row.waitFor();
    assert.doesNotMatch(await row.innerText(), /account-native|openai-codex|credential_generation/);
    assert.match(await row.innerText(), /尚未读取/);
    assert.equal(await page.locator('.provider-detail-workspace').count(), 0);
    assert.equal(await page.locator('.provider-directory details').count(), 0);
    const artifacts = `${root}/e2e-artifacts/ui-system/account-workspace`;
    await mkdir(artifacts, { recursive: true });
    for (const [theme, width] of [['light', 1440], ['dark', 390]] as const) {
      await page.setViewportSize({ width, height: 1000 });
      await page.evaluate(theme => { document.documentElement.dataset.theme = theme; }, theme);
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > document.documentElement.clientWidth), false);
      await page.screenshot({ path: `${artifacts}/list-${theme}-${width}.png`, fullPage: true });
      await row.getByRole('button', { name: '管理账号', exact: true }).click();
      await page.locator('.provider-detail-workspace').waitFor();
      const endpointLabel = page.locator('.provider-detail-workspace .upstream-connection').getByText('上游 API 地址（Base URL）', { exact: true });
      assert.equal(await endpointLabel.count(), 1, 'endpoint help belongs to its field label, not a duplicate row');
      await endpointLabel.focus();
      await page.getByRole('tooltip').waitFor();
      assert.match(await page.getByRole('tooltip').innerText(), /网络出口在账号网络代理中配置/);
      await page.keyboard.press('Escape');
      assert.equal(await page.locator('.provider-directory details').count(), 0);
      assert.equal(await page.evaluate(() => window.formJourneyWrites), 0);
      await page.screenshot({ path: `${artifacts}/detail-${theme}-${width}.png`, fullPage: true });
      await page.locator('.provider-detail-workspace').getByRole('button', { name: '返回账号列表', exact: true }).click();
      assert.equal(await page.locator('.provider-detail-workspace').count(), 0, 'returning unmounts account details');
      assert.equal(await row.locator('[data-manage-account-trigger]').evaluate(button => button === document.activeElement), true);
    }
    await row.getByRole('button', { name: '管理账号', exact: true }).click();
    await page.getByRole('button', { name: '配置网络代理', exact: true }).click();
    await page.locator('.upstream-proxy-editor input').fill('socks5h://10.0.0.20:1080');
    assert.equal(await page.locator('.provider-detail-workspace').getByRole('button', { name: '返回账号列表', exact: true }).isEnabled(), false);
    assert.equal(await page.locator('.provider-detail-workspace').getByRole('button', { name: '编辑', exact: true }).isEnabled(), false);
    await page.getByRole('button', { name: '取消', exact: true }).click();
    await page.locator('.provider-detail-workspace').getByRole('button', { name: '返回账号列表', exact: true }).click();
    await page.setViewportSize({ width: 1440, height: 1000 });
    await row.getByRole('button', { name: '管理账号', exact: true }).click();
    await page.locator('.create-journey [data-workspace-toggle]').click();
    assert.equal(await page.locator('.provider-detail-workspace').count(), 0, 'creating uses the sole work area');
    assert.equal(await row.isVisible(), false, 'the directory is hidden while creating');
    await page.locator('.create-journey [data-workspace-toggle]').click();
    await editProviderAccount(page);
    assert.equal(await page.locator('.provider-detail-workspace').count(), 0, 'editing replaces the account detail renderer');
    assert.equal(await row.isVisible(), false, 'another detail cannot replace an unsaved edit');
    const workspace = page.locator('.provider-edit-workspace');
    const originalProxy = 'socks5h://fixture-user:fixture-password@10.0.0.15:1080';
    const proxyValue = workspace.locator('.provider-proxy-value input');
    await proxyValue.waitFor();
    assert.equal(await proxyValue.inputValue(), originalProxy);
    assert.equal(await workspace.locator('form form').count(), 0, 'connection settings share one form without nested forms');
    assert.equal(await workspace.locator('form .upstream-connection').count(), 1, 'connection settings are inside the account form');
    assert.equal(await proxyValue.getAttribute('value'), null, 'the original is not serialized into HTML');
    assert.equal(await proxyValue.getAttribute('type'), 'text', 'authorized proxy values are directly visible by default');
    await workspace.getByRole('button', { name: '隐藏代理地址', exact: true }).click();
    assert.equal(await proxyValue.getAttribute('type'), 'password', 'hiding is an explicit option');
    await workspace.getByRole('button', { name: '查看代理地址', exact: true }).click();
    await page.context().grantPermissions(['clipboard-read', 'clipboard-write']);
    await workspace.getByRole('button', { name: '复制代理地址', exact: true }).click();
    assert.equal(await page.evaluate(() => navigator.clipboard.readText()), originalProxy);
    const name = workspace.getByLabel('备注名称', { exact: false });
    await name.fill('保留名称草稿');
    assert.equal(await proxyValue.getAttribute('type'), 'text', 'editing another field does not conceal an authorized configuration value');
    await page.getByRole('button', { name: '配置网络代理', exact: true }).click();
    assert.equal(await page.locator('.upstream-proxy-editor input').inputValue(), originalProxy, 'editing starts with the actual current address');
    assert.equal(await page.locator('.upstream-proxy-editor input').getAttribute('type'), 'text', 'proxy edits are directly readable');
    const save = workspace.locator('.rjsf > button[type="submit"]');
    assert.equal(await save.isEnabled(), false);
    await page.locator('.upstream-proxy-editor input').fill('socks5h://10.0.0.10:1080');
    await page.getByRole('button', { name: '保存网络代理', exact: true }).click();
    await page.waitForFunction(() => !document.querySelector<HTMLButtonElement>('.provider-edit-workspace .rjsf > button[type="submit"]')?.disabled);
    assert.equal(await name.inputValue(), '保留名称草稿', 'proxy refresh preserves the independent name draft');
    assert.equal(await page.evaluate(() => window.formJourneyWrites), 1);
    await save.click();
    await page.locator('.provider-detail-workspace').getByRole('heading', { name: '保留名称草稿', exact: true }).waitFor();
    assert.equal(await row.isVisible(), false, 'a successful settings save returns to the account workspace');
    assert.equal(await page.evaluate(() => window.formJourneyWrites), 2, 'provider save succeeds against the new revision without retries');
    await page.evaluate(() => { window.deferNextFormProxyRead = true; });
    await editProviderAccount(page);
    await page.waitForFunction(() => !window.deferNextFormProxyRead);
    await workspace.getByRole('button', { name: '配置网络代理', exact: true }).click();
    await workspace.locator('.upstream-proxy-editor input').fill('socks5h://10.0.0.40:1080');
    await workspace.getByRole('button', { name: '保存网络代理', exact: true }).click();
    await workspace.locator('.provider-proxy-value input').waitFor();
    await page.evaluate(() => window.releaseFormProxyRead());
    await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
    assert.equal(await workspace.locator('.provider-proxy-value input').inputValue(), 'socks5h://10.0.0.40:1080', 'a late old-generation read cannot restore the previous proxy');
    await page.goto(`${origin}/e2e/fixtures/form-journey.html?workflows&proxy-no-authority`);
    await editProviderAccount(page);
    assert.equal(await workspace.locator('.provider-proxy-value input').count(), 0);
    assert.equal(await workspace.getByRole('button', { name: '查看代理地址', exact: true }).count(), 0);
    assert.equal(await page.evaluate(() => window.formJourneyReads.filter(path => path.endsWith('/transport-proxy')).length), 0, 'an account without management capability never requests the original');
    // Credential-generation change invalidates both the cached summary and
    // reset capability. Unknown discovery remains visible, but cannot prepare
    // a reset. These reads and the proxy update are in-memory only; no reset
    // endpoint (not even preparation) is allowed by this fixture.
    await page.goto(`${origin}/e2e/fixtures/form-journey.html?workflows&proxy-workflow&quota-generation`);
    // The fixture exposes one available reset credit with a known expiry.
    await row.getByRole('button', { name: '刷新额度', exact: true }).click();
    await page.waitForFunction(() => window.formJourneyReads.filter(path => path.endsWith('/quota')).length === 1);
    assert.match(await row.innerText(), /重置于/);
    assert.doesNotMatch(await row.innerText(), /重置机会最近到期/);
    const quotaSummary = row.locator('.quota-summary');
    await quotaSummary.focus();
    const quotaTooltip = page.getByRole('tooltip');
    await quotaTooltip.waitFor();
    const quotaTooltipText = await quotaTooltip.innerText();
    const expectedExpiry = await page.evaluate(() => new Date(Date.now() + 3_600_000).toLocaleString('zh-CN'));
    assert.match(quotaTooltipText, /重置机会最近到期/);
    assert.ok(quotaTooltipText.includes(expectedExpiry), 'the single available reset opportunity expiry is shown in the list tooltip');
    await page.setViewportSize({ width: 1440, height: 1000 });
    await page.evaluate(() => { document.documentElement.dataset.theme = 'light'; });
    await page.screenshot({ path: `${artifacts}/quota-summary-tooltip-light-1440.png`, fullPage: true });
    await page.keyboard.press('Escape');
    assert.equal(await page.locator('.provider-detail-workspace').count(), 0, 'list quota refresh does not require opening account details');
    await row.getByRole('button', { name: '管理账号', exact: true }).click();
    await page.locator('.upstream-quota-windows').getByText('Codex 附加用量（代次 1 额度） · 供应商窗口', { exact: true }).waitFor();
    assert.match(await row.innerText(), /75/);
    assert.equal(await quotaDetails.getByRole('region', { name: '额度重置', exact: true }).count(), 1);
    assert.equal(await page.getByRole('button', { name: '重置上游额度', exact: true }).isVisible(), true);
    await page.locator('.provider-detail-workspace').getByRole('button', { name: '返回账号列表', exact: true }).click();
    await row.getByRole('button', { name: '管理账号', exact: true }).click();
    await page.locator('.upstream-quota-windows').getByText('Codex 附加用量（代次 1 额度） · 供应商窗口', { exact: true }).waitFor();
    await page.evaluate(() => { window.deferNextFormQuotaRead = true; });
    await quotaDetails.getByRole('button', { name: '刷新额度', exact: true }).click();
    await page.waitForFunction(() => window.formJourneyReads.filter(path => path.endsWith('/quota')).length === 2);
    await page.getByRole('button', { name: '配置网络代理', exact: true }).click();
    await page.locator('.upstream-proxy-editor input').fill('socks5h://10.0.0.30:1080');
    await page.getByRole('button', { name: '保存网络代理', exact: true }).click();
    await quotaDetails.getByRole('button', { name: '查看额度', exact: true }).waitFor();
    assert.match(await row.innerText(), /尚未读取/);
    assert.equal(await page.locator('.upstream-quota-windows').getByText('Codex 附加用量（代次 1 额度） · 供应商窗口', { exact: true }).count(), 0);
    assert.equal(await quotaDetails.getByRole('region', { name: '额度重置', exact: true }).count(), 1);
    assert.equal(
      await page.getByRole('button', { name: '重置上游额度', exact: true }).isEnabled(),
      false,
      'unknown capability must not allow reset preparation after a credential-generation change',
    );
    await quotaDetails.getByRole('button', { name: '查看额度', exact: true }).click();
    await page.locator('.upstream-quota-windows').getByText('Codex 附加用量（代次 2 额度） · 供应商窗口', { exact: true }).waitFor();
    await page.evaluate(() => window.releaseFormQuotaRead());
    await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
    assert.match(await row.innerText(), /25/);
    assert.doesNotMatch(await row.innerText(), /75/);
    assert.equal(await page.locator('.upstream-quota-windows').getByText('Codex 附加用量（代次 1 额度） · 供应商窗口', { exact: true }).count(), 0);
    assert.equal(await quotaDetails.getByRole('button', { name: '重置上游额度', exact: true }).count(), 0, 'unsupported new generation exposes no reset action');
    await page.locator('.provider-detail-workspace').getByRole('button', { name: '返回账号列表', exact: true }).click();
    await row.getByRole('button', { name: '管理账号', exact: true }).click();
    await page.locator('.upstream-quota-windows').getByText('Codex 附加用量（代次 2 额度） · 供应商窗口', { exact: true }).waitFor();
    assert.equal(await page.evaluate(() => window.formJourneyWrites), 1, 'only the mocked proxy change was written; no reset operation was attempted');
    assert.deepEqual(errors, []);
  } finally { await browser.close(); await server.close(); }
});

test('account action slots fit English Fluent buttons through the sidebar breakpoint', async () => {
  const root = fileURLToPath(new URL('..', import.meta.url));
  const server = await createIsolatedFixtureServer({ root, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const origin = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  const artifacts = `${root}/e2e-artifacts/ui-system/account-workspace`;
  await mkdir(artifacts, { recursive: true });
  try {
    const page = await browser.newPage();
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    const errors: string[] = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.route('**/*', route => {
      const url = new URL(route.request().url());
      return url.origin === origin && !url.pathname.startsWith('/internal/') ? route.continue() : route.abort();
    });
    await page.goto(`${origin}/e2e/fixtures/form-journey.html?workflows`);
    await page.evaluate(async () => { document.documentElement.dataset.theme = 'light'; await document.fonts.ready; });
    const actions = page.locator('.provider-directory-actions > .fui-Button');
    await actions.first().waitFor();
    for (const width of [1440, 1280, 1101, 1100, 1099, 900, 390, 320]) {
      const height = width <= 768 ? 844 : 1000;
      await page.setViewportSize({ width, height });
      const dimensions = await actions.evaluateAll(buttons => buttons.map(button => {
        const bounds = button.getBoundingClientRect();
        const style = getComputedStyle(button);
        return { left: bounds.left, top: bounds.top, bottom: bounds.bottom, width: bounds.width, height: bounds.height, textLength: button.textContent?.trim().length ?? 0, scrollWidth: button.scrollWidth, clientWidth: button.clientWidth, fontSize: style.fontSize, paddingInlineStart: style.paddingInlineStart, paddingInlineEnd: style.paddingInlineEnd };
      }));
      const stem = `${artifacts}/accounts-list--action-slots--en--${width}x${height}--light`;
      await writeFile(`${stem}.json`, JSON.stringify({ evidence_kind: 'synthetic', integrated_head_sha: process.env.GITHUB_SHA ?? null, viewport: { width, height }, actions: dimensions }, null, 2));
      await page.screenshot({ path: `${stem}.png`, fullPage: true });
      assert.equal(await actions.nth(0).innerText(), 'Refresh quota');
      assert.equal(await actions.nth(1).innerText(), 'Manage account');
      assert.equal(dimensions.length, 2);
      if (width === 320) {
        assert.ok(dimensions[1].top >= dimensions[0].bottom, 'narrow common actions use ordered full-width slots instead of wrapping their labels');
        assert.ok(Math.abs(dimensions[0].left - dimensions[1].left) <= 1);
        assert.ok(Math.abs(dimensions[0].width - dimensions[1].width) <= 1);
      }
      for (const action of dimensions) {
        assert.equal(action.height, width <= 768 ? 44 : 32, `${width}px common actions retain the shared Fluent height`);
        assert.ok(action.scrollWidth <= action.clientWidth, `${width}px action text remains contained`);
      }
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true, `${width}px sidebar and directory do not overflow`);
    }
    assert.deepEqual(errors, []);
  } finally { await browser.close(); await server.close(); }
});

test('account deletion clears only its workspace and ordinary tooltips omit technical identifiers', { timeout: 120_000 }, async () => {
  const root = fileURLToPath(new URL('..', import.meta.url));
  const server = await createIsolatedFixtureServer({ root, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const origin = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  const artifacts = `${root}/e2e-artifacts/ui-system/account-workspace`;
  await mkdir(artifacts, { recursive: true });
  try {
    for (const locale of ['zh-CN', 'en'] as const) for (const width of [1440, 390]) {
      const height = width === 390 ? 844 : 1000;
      const page = await browser.newPage({ viewport: { width, height } });
      await page.addInitScript(value => localStorage.setItem('mtc-locale', value), locale);
      const chinese = locale === 'zh-CN';
      const firstId = '11111111-1111-4111-8111-111111111111';
      const secondId = '22222222-2222-4222-8222-222222222222';
      const errors: string[] = [];
      page.on('pageerror', error => errors.push(error.message));
      let deleted = false;
      let deletionMode: 'failed' | 'ready' | 'late' = 'failed';
      let releaseDelete: (() => void) | undefined;
      let deleteStarted: (() => void) | undefined;
      const account = (id: string, tenant: string) => ({ id, tenant_external_id: tenant, name: id === firstId ? 'Primary account' : 'Second account', driver: 'http-json', auth_kind: 'api_key', connection_method: 'api_key', credential_generation: 7, credential_expires_at: null, status: 'active', can_rotate: false, can_reauthorize: false, can_refresh: false, route_count: 0, config: {}, created_at: 1, updated_at: 2 });
      await page.route('**/*', async route => {
        const request = route.request(); const url = new URL(request.url());
        assert.equal(url.origin, origin, 'deletion contracts only use the isolated fixture');
        if (!url.pathname.startsWith('/internal/')) return route.continue();
        if (request.method() !== 'GET') {
          assert.equal(request.method(), 'DELETE');
          assert.equal(url.pathname, `/internal/v1/upstreams/${firstId}`);
          assert.equal(url.searchParams.get('tenant_external_id'), 'fixture-a');
          assert.equal(url.searchParams.get('expected_updated_at'), '2');
          if (deletionMode === 'failed') return route.fulfill({ status: 409, json: { error: { message: 'Fixture deletion rejected' } } });
          if (deletionMode === 'late') await new Promise<void>(resolve => { releaseDelete = resolve; deleteStarted?.(); });
          deleted = true;
          return route.fulfill({ json: {} });
        }
        if (url.pathname === '/internal/v1/upstreams') {
          const tenant = url.searchParams.get('tenant_external_id') ?? 'fixture-a';
          return route.fulfill({ json: [...(!deleted && tenant === 'fixture-a' ? [account(firstId, tenant)] : []), account(secondId, tenant)] });
        }
        if (url.pathname === '/internal/v1/provider-types') return route.fulfill({ json: [{ id: 'http-json', display_name: 'Fixture provider', source: 'builtin', protocols: ['openai'], modalities: ['text'], config_schema: { type: 'object', properties: {} }, credential_schema: { type: 'object', properties: {} } }] });
        if (url.pathname.endsWith('/deletion-readiness')) return route.fulfill({ json: { can_delete: true, requires_disabled: false, model_route_count: 0, imported_for_audit: false } });
        if (url.pathname.endsWith('/models')) return route.fulfill({ json: { account_id: url.pathname.split('/')[4], credential_generation: 7, status: 'ready', models: [], disabled_models: [] } });
        if (url.pathname.endsWith('/access')) return route.fulfill({ json: { can_manage: false } });
        if (url.pathname.includes('monitoring')) return route.fulfill({ json: { top_upstream_models: [] } });
        if (url.pathname.includes('availability')) return route.fulfill({ json: { tenant_external_id: 'fixture-a', accounts: [] } });
        return route.fulfill({ json: [] });
      });
      const fixture = `${origin}/e2e/fixtures/authorization-code.html?scope-controls`;
      await page.goto(fixture);
      await page.locator(`[data-upstream-id="${firstId}"] .provider-directory-identity b`).hover();
      await page.getByRole('tooltip').last().waitFor();
      for (const text of await page.getByRole('tooltip').allTextContents()) {
        assert.equal(text.includes(firstId), false);
        assert.equal(text.includes('http-json'), false);
      }
      const details = await manageProviderAccount(page, firstId);
      const accountInformation = details.locator('.provider-detail-heading').getByText(chinese ? '账户信息' : 'Account information', { exact: true });
      assert.equal(await accountInformation.getAttribute('tabindex'), '0', 'the visible account information label owns the tooltip');
      await accountInformation.hover();
      await page.getByRole('tooltip').last().waitFor();
      for (const text of await page.getByRole('tooltip').allTextContents()) {
        assert.equal(text.includes(firstId), false);
        assert.equal(text.includes('http-json'), false);
      }
      assert.equal((await details.locator('.provider-detail-heading').innerText()).includes(firstId), false);
      await details.getByRole('button', { name: chinese ? '技术详情' : 'Technical details', exact: true }).click();
      await details.locator('code').filter({ hasText: firstId }).waitFor();
      assert.equal(await details.getByRole('button', { name: chinese ? '复制账号 ID' : 'Copy account ID', exact: true }).count(), 1);
      await details.getByRole('button', { name: chinese ? '危险操作' : 'Danger zone', exact: true }).click();
      const remove = details.getByRole('button', { name: chinese ? '删除' : 'Remove', exact: true });
      const proceed = page.getByRole('dialog').getByRole('button', { name: chinese ? '确认继续' : 'Confirm and continue', exact: true });
      await remove.click(); await proceed.click();
      await details.getByRole('alert').filter({ hasText: 'Fixture deletion rejected' }).waitFor();
      assert.equal(await details.count(), 1, 'a rejected deletion retains the account workspace');
      deletionMode = 'ready';
      await remove.click(); await proceed.click();
      await details.waitFor({ state: 'detached' });
      await page.locator(`[data-upstream-id="${firstId}"]`).waitFor({ state: 'detached' });
      assert.equal(await page.locator('.provider-detail-workspace').count(), 0, 'deleted accounts never remain as fallback details');
      await page.locator(`[data-manage-account-trigger="${secondId}"]`).waitFor();
      await page.screenshot({ path: `${artifacts}/accounts-list--deleted--${locale}--${width}x${height}--light.png`, fullPage: true });
      deleted = false; deletionMode = 'late';
      await page.goto(fixture);
      await manageProviderAccount(page, firstId);
      await details.getByRole('button', { name: chinese ? '危险操作' : 'Danger zone', exact: true }).click();
      const pending = new Promise<void>(resolve => { deleteStarted = resolve; });
      await remove.click(); await proceed.click(); await pending;
      await page.getByRole('button', { name: 'Switch tenant', exact: true }).click();
      const secondDetails = await manageProviderAccount(page, secondId);
      const completed = page.waitForResponse(response => response.request().method() === 'DELETE');
      assert.ok(releaseDelete); releaseDelete(); await completed;
      await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
      assert.equal(await secondDetails.isVisible(), true, 'late deletion of A cannot close the new B workspace');
      assert.equal(await secondDetails.getByRole('heading', { name: 'Second account', exact: true }).count(), 1);
      assert.equal(await page.locator('.notice.success').count(), 0, 'late deletion does not publish into another scope');
      assert.deepEqual(errors, []);
      await page.close();
    }
  } finally { await browser.close(); await server.close(); }
});

test('account workspace returns without retained details, shares list tracks and rejects late saves across scopes', { timeout: 120_000 }, async () => {
  const root = fileURLToPath(new URL('..', import.meta.url));
  const server = await createIsolatedFixtureServer({ root, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const origin = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  const artifacts = `${root}/e2e-artifacts/ui-system/account-workspace`;
  await mkdir(artifacts, { recursive: true });
  try {
    for (const locale of ['zh-CN', 'en'] as const) {
      const chinese = locale === 'zh-CN';
      const page = await browser.newPage({ viewport: { width: 1440, height: 1000 } });
      await page.addInitScript(value => localStorage.setItem('mtc-locale', value), locale);
      const errors: string[] = [];
      page.on('pageerror', error => errors.push(error.message));
      const labels = { edit: chinese ? '编辑' : 'Edit', close: chinese ? '关闭' : 'Close', back: chinese ? '返回账号列表' : 'Back to account list', name: chinese ? '备注名称' : 'Display name', cancel: chinese ? '取消' : 'Cancel', proceed: chinese ? '确认继续' : 'Confirm and continue' };
      const accounts = (tenant: string) => [
        { id: `account-${tenant}`, tenant_id: `tenant-${tenant}`, tenant_external_id: tenant, name: 'Short account', driver: 'http-json', auth_kind: 'api_key', connection_method: 'api_key', credential_generation: 1, credential_expires_at: null, status: 'active', can_rotate: true, can_reauthorize: false, can_refresh: false, route_count: 1234, config: { base_url: 'https://provider.example.invalid' }, created_at: 1, updated_at: 2 },
        { id: `expired-${tenant}`, tenant_id: `tenant-${tenant}`, tenant_external_id: tenant, name: 'Long account name 中文账号名称 '.repeat(5), driver: 'kimi-oauth', auth_kind: 'oauth', connection_method: 'oauth', credential_generation: 1, credential_expires_at: 1, status: 'active', can_rotate: false, can_reauthorize: true, can_refresh: true, route_count: 0, config: {}, created_at: 1, updated_at: 2 },
        { id: `retired-${tenant}`, tenant_id: `tenant-${tenant}`, tenant_external_id: tenant, name: 'VeryLongUnbrokenAccountName'.repeat(6), driver: 'retired-fixture', auth_kind: 'api_key', connection_method: 'api_key', credential_generation: 1, credential_expires_at: null, status: 'active', can_rotate: false, can_reauthorize: false, can_refresh: false, route_count: 99, config: {}, created_at: 1, updated_at: 2 },
      ];
      let state: 'normal' | 'empty' | 'denied' | 'slow' = 'normal';
      let releaseRead: (() => void) | undefined;
      let releaseSave: (() => void) | undefined;
      let readStarted: (() => void) | undefined;
      let saveStarted: (() => void) | undefined;
      let writes = 0;
      let deferSave = false;
      let failNextListRead = false;
      let savedAccount: ReturnType<typeof accounts>[number] | undefined;
      await page.route('**/*', async route => {
        const request = route.request(); const url = new URL(request.url());
        assert.equal(url.origin, origin, 'account contracts never contact a live provider');
        if (!url.pathname.startsWith('/internal/')) return route.continue();
        if (request.method() !== 'GET') {
          assert.equal(request.method(), 'PUT');
          assert.equal(url.pathname, '/internal/v1/upstreams/account-fixture-a');
          writes += 1;
          if (deferSave) await new Promise<void>(resolve => { releaseSave = resolve; saveStarted?.(); saveStarted = undefined; });
          const updated = { ...accounts('fixture-a')[0], name: deferSave ? 'Late saved account' : request.postDataJSON().name, updated_at: 3 };
          if (!deferSave) savedAccount = updated;
          return route.fulfill({ json: updated });
        }
        if (url.pathname === '/internal/v1/upstreams') {
          const tenant = url.searchParams.get('tenant_external_id') ?? 'fixture-a';
          if (failNextListRead) { failNextListRead = false; return route.fulfill({ status: 503, json: { error: { message: 'Fixture list refresh unavailable' } } }); }
          if (state === 'slow') await new Promise<void>(resolve => { releaseRead = resolve; readStarted?.(); readStarted = undefined; });
          if (state === 'denied') return route.fulfill({ status: 403, json: { error: { message: 'Fixture account read denied' } } });
          return route.fulfill({ json: state === 'empty' ? [] : accounts(tenant).map(account => savedAccount?.id === account.id ? savedAccount : account) });
        }
        if (url.pathname === '/internal/v1/provider-types') return route.fulfill({ json: [
          { id: 'http-json', display_name: 'Fixture provider', source: 'builtin', protocols: ['openai'], modalities: ['text'], config_schema: { type: 'object', properties: { base_url: { type: 'string' } } }, credential_schema: { type: 'object', properties: { type: { const: 'api_key' }, value: { type: 'string', writeOnly: true } } } },
          { id: 'kimi-oauth', display_name: 'Fixture Kimi', source: 'builtin', protocols: ['openai'], modalities: ['text'], config_schema: { type: 'object', properties: {} }, credential_schema: { type: 'object', properties: { type: { const: 'oauth' } } }, oauth_adapter: { flow_kind: 'kimi_device' } },
        ] });
        if (url.pathname.endsWith('/models')) return route.fulfill({ json: { account_id: url.pathname.split('/')[4], credential_generation: 1, status: 'ready', last_attempt_at: null, last_success_at: null, expires_at: null, error_code: null, models: [], disabled_models: [] } });
        if (url.pathname.endsWith('/access')) return route.fulfill({ json: { can_manage: false } });
        if (url.pathname.includes('monitoring')) return route.fulfill({ json: { top_upstream_models: [] } });
        if (url.pathname.includes('availability')) return route.fulfill({ json: { tenant_external_id: 'fixture-a', accounts: [] } });
        return route.fulfill({ json: [] });
      });
      const fixture = `${origin}/e2e/fixtures/authorization-code.html?scope-controls`;
      await page.goto(fixture);
      const rows = page.locator('.provider-directory-row');
      await rows.nth(2).waitFor();
      const expiredRow = page.locator('[data-upstream-id="expired-fixture-a"]');
      assert.equal(await expiredRow.getByText(chinese ? '授权已过期' : 'Authorization expired', { exact: true }).count(), 1, 'enabled Kimi accounts still expose credential expiry');
      assert.equal(await expiredRow.locator('.status.ok').count(), 0, 'enabled is not proof of valid authorization');
      for (const width of [1440, 1101, 1100, 900, 390, 320]) {
        await page.setViewportSize({ width, height: 1000 });
        const tracks = await rows.evaluateAll(elements => elements.map(row => [...row.children].slice(0, 5).map(cell => { const box = cell.getBoundingClientRect(); return { left: box.left, width: box.width }; })));
        for (const cells of tracks.slice(1)) for (const [index, cell] of cells.entries()) {
          assert.ok(Math.abs(cell.left - tracks[0][index].left) < 1, `${locale} ${width}px column ${index} shares its left boundary`);
          assert.ok(Math.abs(cell.width - tracks[0][index].width) < 1, `${locale} ${width}px column ${index} shares its width`);
        }
        const actions = await rows.evaluateAll(elements => elements.map(row => Array.from(row.querySelectorAll('.provider-directory-actions > button'), button => {
          const bounds = button.getBoundingClientRect();
          return { left: bounds.left, top: bounds.top, bottom: bounds.bottom };
        })));
        assert.deepEqual(actions.map(buttons => buttons.length), [2, 3, 2], 'reauthorization remains available in addition to both common actions');
        for (const buttons of actions.slice(1)) for (const index of [0, 1]) {
          assert.ok(Math.abs(buttons[index].left - actions[0][index].left) <= 1, `${locale} ${width}px common action ${index} retains its slot`);
        }
        assert.ok(actions[1][2].top >= Math.max(actions[1][0].bottom, actions[1][1].bottom), 'the additional authorization action occupies its own next row');
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true, `${locale} ${width}px long names and actions reflow`);
        await page.screenshot({ path: `${artifacts}/normal-${locale}-${width}.png`, fullPage: true });
      }
      const expiredDetails = await manageProviderAccount(page, 'expired-fixture-a');
      await expiredDetails.locator('.provider-model-catalog').getByText(chinese ? '已同步' : 'Synced', { exact: false }).waitFor();
      assert.equal(await expiredDetails.locator('.account-main > .status').innerText(), chinese ? '授权已过期' : 'Authorization expired', 'a ready catalog never overrides an expired Kimi credential');
      assert.equal(await expiredDetails.locator('.account-main > .status.ok').count(), 0);
      await page.screenshot({ path: `${artifacts}/expired-kimi-${locale}-320.png`, fullPage: true });
      await expiredDetails.getByRole('button', { name: labels.back, exact: true }).click();
      const manage = page.locator('[data-manage-account-trigger="account-fixture-a"]');
      const details = await manageProviderAccount(page, 'account-fixture-a');
      assert.equal(await rows.first().isVisible(), false, 'account details and the directory are mutually exclusive');
      assert.equal(await page.locator('.provider-directory .provider-detail-workspace').count(), 0, 'details have one standalone mount point');
      assert.equal(await details.locator('.provider-model-catalog').count(), 1);
      await details.getByRole('button', { name: labels.edit, exact: true }).click();
      const editor = page.locator('.provider-edit-workspace');
      assert.equal(await details.count(), 0, 'settings unmount details rather than concealing them');
      assert.equal(await editor.locator('.provider-model-catalog').count(), 1);
      const name = editor.getByLabel(labels.name, { exact: false });
      await name.fill('Unsaved account draft');
      await editor.getByRole('button', { name: labels.close, exact: true }).click();
      const dialog = page.getByRole('dialog');
      await dialog.getByText(providerConnectionCopy(locale).discard, { exact: true }).waitFor();
      await dialog.getByRole('button', { name: labels.cancel, exact: true }).click();
      assert.equal(await name.inputValue(), 'Unsaved account draft');
      assert.equal(await details.count(), 0);
      await editor.getByRole('button', { name: labels.close, exact: true }).click();
      await dialog.getByRole('button', { name: labels.proceed, exact: true }).click();
      await details.waitFor();
      assert.equal(await editor.count(), 0);
      assert.equal(await details.getByRole('button', { name: labels.edit, exact: true }).evaluate(button => button === document.activeElement), true);
      await details.getByRole('button', { name: labels.back, exact: true }).click();
      assert.equal(await details.count(), 0);
      assert.equal(await manage.evaluate(button => button === document.activeElement), true);
      await page.screenshot({ path: `${artifacts}/returned-${locale}-320.png`, fullPage: true });
      await page.setViewportSize({ width: 1440, height: 1000 });
      await page.screenshot({ path: `${artifacts}/returned-${locale}-1440.png`, fullPage: true });
      await page.setViewportSize({ width: 320, height: 1000 });
      for (const refreshFailure of [false, true]) {
        await editProviderAccount(page, 'account-fixture-a');
        await name.fill(refreshFailure ? 'Saved despite refresh failure' : 'Saved account workspace');
        failNextListRead = refreshFailure;
        const failedRead = refreshFailure ? page.waitForResponse(response => new URL(response.url()).pathname === '/internal/v1/upstreams' && response.request().method() === 'GET' && response.status() === 503) : undefined;
        await editor.locator('.rjsf > button[type="submit"]').click();
        await details.getByRole('heading', { name: refreshFailure ? 'Saved despite refresh failure' : 'Saved account workspace', exact: true }).waitFor();
        assert.equal(await editor.count(), 0, 'saving returns to the same account layer');
        assert.equal(await rows.first().isVisible(), false);
        await details.getByRole('status').filter({ hasText: chinese ? '已更新' : 'Updated' }).waitFor();
        if (refreshFailure) {
          await failedRead;
          await page.getByRole('alert').filter({ hasText: chinese ? '账号已保存。重新读取列表即可查看。' : 'Account saved. Reload the list to view it.' }).waitFor();
          assert.equal(await page.getByRole('alert').count(), 1, 'a failed read after a successful write has one retryable feedback owner');
          assert.equal(await details.getByRole('alert').count(), 0, 'a successful save is not reported as a failed write');
          assert.equal(await page.getByText('Fixture list refresh unavailable', { exact: true }).count(), 0, 'raw read errors are not exposed');
          assert.equal(await details.getByRole('heading', { name: 'Saved despite refresh failure', exact: true }).count(), 1, 'failed directory refresh retains the saved account snapshot');
        }
        for (const width of [1440, 390]) {
          await page.setViewportSize({ width, height: 1000 });
          await page.screenshot({ path: `${artifacts}/saved-${refreshFailure ? 'refresh-failed' : 'ready'}-${locale}-${width}.png`, fullPage: true });
        }
        if (refreshFailure) {
          const writesBeforeRetry = writes;
          const refreshed = page.waitForResponse(response => new URL(response.url()).pathname === '/internal/v1/upstreams' && response.request().method() === 'GET' && response.status() === 200);
          await page.getByRole('alert').getByRole('button', { name: chinese ? '重试' : 'Retry', exact: true }).click();
          await refreshed;
          await page.getByRole('alert').waitFor({ state: 'detached' });
          await details.getByRole('heading', { name: 'Saved despite refresh failure', exact: true }).waitFor();
          await details.getByRole('status').filter({ hasText: chinese ? '已更新' : 'Updated' }).waitFor();
          assert.equal(writes, writesBeforeRetry, 'retry only rereads the directory and never repeats the saved mutation');
          assert.equal(await rows.first().isVisible(), false);
        }
      }
      await details.getByRole('button', { name: labels.back, exact: true }).click();
      assert.equal(await details.count(), 0);
      deferSave = true;
      await page.setViewportSize({ width: 320, height: 1000 });
      for (const control of ['Switch tenant', 'Switch credential', 'Switch write tenant']) {
        await page.goto(fixture);
        await editProviderAccount(page, 'account-fixture-a');
        const submitted = new Promise<void>(resolve => { saveStarted = resolve; });
        await editor.locator('.rjsf > button[type="submit"]').click();
        await submitted;
        assert.ok(releaseSave, 'the mocked save is pending before the scope changes');
        await page.getByRole('button', { name: control, exact: true }).click();
        await editor.waitFor({ state: 'detached' });
        const response = page.waitForResponse(value => value.request().method() === 'PUT');
        releaseSave(); releaseSave = undefined;
        await response;
        await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
        assert.equal(await details.count(), 0, `${control} cannot reopen the old account`);
        assert.equal(await editor.count(), 0, `${control} cannot restore the old editor`);
        assert.equal(await page.getByText('Late saved account', { exact: true }).count(), 0);
        assert.equal(await page.locator('.notice.success').count(), 0);
        if (control === 'Switch write tenant') {
          await manage.click();
          assert.equal(await details.getByRole('button', { name: labels.edit, exact: true }).isDisabled(), true);
          await page.screenshot({ path: `${artifacts}/permission-${locale}-320.png`, fullPage: true });
          await page.setViewportSize({ width: 1440, height: 1000 });
          await page.screenshot({ path: `${artifacts}/permission-${locale}-1440.png`, fullPage: true });
          await page.setViewportSize({ width: 320, height: 1000 });
          await page.getByRole('button', { name: control, exact: true }).click();
          assert.equal(await details.count(), 0, 'restoring write authority does not reopen the old account');
        }
      }
      assert.equal(writes, 5, 'only explicitly submitted mocked settings are written');
      for (const width of [1440, 390]) {
        await page.setViewportSize({ width, height: 1000 });
        state = 'empty'; await page.goto(fixture);
        await page.getByText(chinese ? '暂无上游提供商' : 'No upstream providers', { exact: true }).waitFor();
        await page.screenshot({ path: `${artifacts}/empty-${locale}-${width}.png`, fullPage: true });
        state = 'denied'; await page.goto(fixture);
        await page.getByRole('alert').getByText('Fixture account read denied', { exact: false }).waitFor();
        await page.screenshot({ path: `${artifacts}/denied-${locale}-${width}.png`, fullPage: true });
        state = 'slow';
        const reading = new Promise<void>(resolve => { readStarted = resolve; });
        await page.goto(fixture); await reading;
        await page.getByText(chinese ? '载入中…' : 'Loading…', { exact: true }).waitFor();
        assert.ok(releaseRead);
        await page.screenshot({ path: `${artifacts}/slow-${locale}-${width}.png`, fullPage: true });
        state = 'normal'; releaseRead(); releaseRead = undefined;
        await rows.nth(2).waitFor();
        assert.equal(await details.count(), 0);
      }
      assert.deepEqual(errors, []);
      await page.close();
    }
  } finally { await browser.close(); await server.close(); }
});
