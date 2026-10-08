import assert from 'node:assert/strict';
import { editProviderAccount } from './support/provider-account-navigation.js';
import { mkdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer as createServer } from './support/isolated-vite-server.js';
import { fixtureAssets } from './support/fixture-assets.js';

test('AppShell workspaces retain failed drafts, return after success, and prioritize the one-time credential', { timeout: 60_000 }, async () => {
  const root = fileURLToPath(new URL('..', import.meta.url));
  const server = await createServer({ root, configFile: false, plugins: [fixtureAssets()], logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen(); const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const origin = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 1440, height: 1000 } });
    const productionWrites: string[] = [], errors: string[] = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.route('**/*', route => {
      const url = new URL(route.request().url());
      if (url.origin !== origin || url.pathname.startsWith('/internal/')) { if (route.request().method() !== 'GET') productionWrites.push(url.pathname); return route.abort(); }
      return route.continue();
    });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'zh-CN'));
    await page.clock.setFixedTime(new Date('2026-09-13T07:00:00Z'));
    const artifacts = `${root}/e2e-artifacts/form-workspace-round3`; await mkdir(artifacts, { recursive: true });
    await page.goto(`${origin}/e2e/fixtures/form-journey.html?view=routes&workflows=1`);
    await page.getByRole('button', { name: '编辑', exact: true }).click();
    const workspace = page.locator('.create-journey');
    assert.equal(await page.locator('.management-layout > article:not(.create-journey) .inline-editor').count(), 0);
    const name = workspace.getByLabel(/公开模型/);
    await name.fill('research-model-edited');
    const save = workspace.getByRole('button', { name: '保存', exact: true });
    await page.evaluate(() => { window.failNextFormWrite = true; });
    await save.click();
    await workspace.getByRole('alert').waitFor();
    assert.equal(await name.inputValue(), 'research-model-edited');
    assert.equal(await save.evaluate(element => document.activeElement === element), true, 'network failure restores the submitting control focus');
    await page.screenshot({ path: `${artifacts}/route-edit-failure-1440.png`, fullPage: true });
    await save.click();
    await page.locator('.notice.success').waitFor();
    assert.equal(await workspace.getAttribute('data-open'), 'false');
    assert.equal(await page.locator('.notice.success').evaluate(element => document.activeElement === element), true);
    await workspace.locator('[data-workspace-toggle]').click();
    await workspace.getByLabel(/公开模型/).fill('research-created');
    const account = workspace.getByRole('combobox', { name: '上游账号', exact: true });
    await account.fill('研发订阅'); await account.press('ArrowDown'); await account.press('Enter'); await account.press('Escape');
    const model = workspace.getByRole('combobox', { name: '上游模型', exact: true });
    await model.fill('fixture-model');
    await page.waitForFunction(() => { const button = document.querySelector<HTMLButtonElement>('.create-journey .journey-actions button'); return Boolean(button && !button.disabled); });
    await page.screenshot({ path: `${artifacts}/route-picker-appshell-1440.png`, fullPage: true });
    await model.press('Escape');
    const create = workspace.getByRole('button', { name: '创建路由', exact: true });
    for (const theme of ['light', 'dark']) for (const width of [320, 390, 1440]) {
      await page.evaluate(theme => { document.documentElement.dataset.theme = theme; }, theme);
      await page.setViewportSize({ width, height: 1000 });
      assert.equal(await workspace.locator('.journey-actions').evaluate(element => getComputedStyle(element).position), 'static');
      await create.scrollIntoViewIfNeeded();
      // Trial checks real actionability (including layout settling and hit
      // testing), but never dispatches a click or writes through the fixture.
      await create.click({ trial: true });
      await page.screenshot({ path: `${artifacts}/route-create-appshell-${theme}-${width}.png`, fullPage: true });
    }
    await create.click(); await page.locator('.notice.success').waitFor();
    assert.equal(await workspace.getAttribute('data-open'), 'false');
    await page.setViewportSize({ width: 1440, height: 1000 });
    await page.getByRole('link', { name: '上游服务', exact: true }).click();
    await page.getByText('研发订阅', { exact: true }).waitFor();
    await editProviderAccount(page);
    for (const theme of ['light', 'dark']) for (const width of [320, 1440]) {
      await page.evaluate(theme => { document.documentElement.dataset.theme = theme; }, theme);
      await page.setViewportSize({ width, height: 1000 });
      if (width <= 900) await page.waitForFunction(() => document.querySelector('.app-sidebar')!.getBoundingClientRect().right <= 0);
      await page.screenshot({ path: `${artifacts}/provider-edit-appshell-${theme}-${width}.png`, fullPage: true });
    }
    // Credential fixture intercepts every API call in memory. Fail one create,
    // then return a synthetic one-time credential; no production write exists.
    await page.goto(`${origin}/e2e/fixtures/operator-credential-workspace.html?scenario=client-form`);
    const credential = page.locator('.credential-compact-row').first();
    await credential.waitFor();
    const credentialArtifacts = `${root}/e2e-artifacts/ui-system/credentials`;
    await mkdir(credentialArtifacts, { recursive: true });
    for (const theme of ['light', 'dark']) for (const width of [390, 1440]) {
      await page.evaluate(value => { document.documentElement.dataset.theme = value; }, theme);
      await page.setViewportSize({ width, height: 1000 });
      await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => resolve(null))));
      const identityGap = await credential.evaluate(row => {
        const checkbox = row.querySelector('.fui-Checkbox')!.getBoundingClientRect();
        const identity = row.querySelector('.credential-row-identity')!.getBoundingClientRect();
        return identity.left - checkbox.right;
      });
      assert.ok(identityGap >= 0 && identityGap <= 24, 'identity remains beside its selection, not centered across the page');
      assert.equal(await credential.locator('.credential-row-actions').getByRole('button', { name: '更多操作', exact: true }).count(), 1, 'copy and secondary actions form one action group');
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false);
      await page.screenshot({ path: `${credentialArtifacts}/credential-list-${theme}-${width}.png`, fullPage: true });
    }
    await page.locator('[data-workspace-toggle]').click();
    await page.locator('#root_principal_external_id').fill('fixture-principal');
    await page.locator('#root_alias').fill('研发自动化');
    await page.evaluate(() => {
      const original = window.fetch; let fail = true;
      window.fetch = async (input, init) => {
        if (fail && String(input).endsWith('/internal/v1/keys') && init?.method === 'POST') { fail = false; return new Response(JSON.stringify({ error: { message: '模拟失败，保留草稿' } }), { status: 400 }); }
        return original(input, init);
      };
    });
    const submit = page.locator('.create-journey button[type="submit"]');
    await submit.click(); await page.locator('.create-journey [role="alert"]').waitFor();
    assert.equal(await page.locator('#root_alias').inputValue(), '研发自动化');
    assert.equal(await submit.evaluate(element => document.activeElement === element), true);
    await submit.click(); await page.locator('.credential-secret-priority').waitFor();
    assert.equal(await page.locator('.create-journey').getAttribute('data-open'), 'false');
    assert.equal(await page.locator('.credential-secret-priority').evaluate(element => document.activeElement === element), true);
    // Closing and failed creation preserve a direct-provider draft. Only a
    // successful create replaces its form instance and clears the API secret.
    await page.goto(`${origin}/e2e/fixtures/form-journey.html?workflows&provider-workflow`);
    const providerWorkspace = page.locator('.create-journey');
    const providerToggle = providerWorkspace.locator('[data-workspace-toggle]');
    await providerToggle.click();
    const providerName = providerWorkspace.locator('#root_name');
    const apiKey = providerWorkspace.locator('#root_credential_api_key');
    await providerName.fill('保留的新增上游');
    await providerWorkspace.locator('#root_config_base_url').fill('https://fixture.invalid');
    await apiKey.fill('fixture-only-api-secret');
    await providerToggle.click(); await providerToggle.click();
    assert.equal(await providerName.inputValue(), '保留的新增上游');
    assert.equal(await apiKey.inputValue(), 'fixture-only-api-secret');
    await page.evaluate(() => { window.failNextFormWrite = true; });
    const createProvider = providerWorkspace.getByRole('button', { name: '添加上游', exact: true });
    await createProvider.click();
    await providerWorkspace.getByRole('alert').waitFor();
    assert.equal(await providerName.inputValue(), '保留的新增上游');
    assert.equal(await apiKey.inputValue(), 'fixture-only-api-secret');
    await createProvider.click();
    await page.waitForFunction(() => document.querySelector('.create-journey')?.getAttribute('data-open') === 'false');
    await providerToggle.click();
    assert.equal(await providerName.inputValue(), '');
    assert.equal(await apiKey.inputValue(), '', 'the successful API secret cannot be resubmitted from the next create form');
    assert.equal(await page.evaluate(() => window.formJourneyWrites), 2);
    // Deferred in-memory routing responses exercise same-tick double submits
    // and a late error after closing an editor, without external writes.
    await page.goto(`${origin}/e2e/fixtures/operator-credential-workspace.html?scenario=client-form&routing-lifecycle`);
    await page.getByRole('button', { name: '更多操作', exact: true }).click();
    await page.getByRole('menuitem', { name: '路由权限', exact: true }).click();
    await page.waitForFunction(() => window.credentialFixture.requests.some(request => request.path.includes('/key-form/routing')));
    await page.evaluate(() => window.credentialFixture.releaseRoutingResponse(200));
    const saveRouting = page.locator('.routing-editor').getByRole('button', { name: '保存', exact: true });
    await saveRouting.waitFor();
    await saveRouting.evaluate(button => { (button as HTMLButtonElement).click(); (button as HTMLButtonElement).click(); });
    await page.waitForFunction(() => window.credentialFixture.requests.some(request => request.method === 'PUT'));
    assert.equal(await saveRouting.isEnabled(), false);
    assert.equal(await page.evaluate(() => window.credentialFixture.requests.filter(request => request.method === 'PUT').length), 1);
    await page.evaluate(() => window.credentialFixture.releaseRoutingResponse(200));
    await page.waitForFunction(() => document.querySelector('.create-journey')?.getAttribute('data-open') === 'false');
    await page.getByRole('button', { name: '更多操作', exact: true }).click();
    await page.getByRole('menuitem', { name: '路由权限', exact: true }).click();
    await page.waitForFunction(() => window.credentialFixture.requests.filter(request => request.method === 'GET' && request.path.includes('/key-form/routing')).length === 2);
    await page.locator('.create-journey [data-workspace-toggle]').click();
    await page.evaluate(() => window.credentialFixture.releaseRoutingResponse(409));
    await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
    assert.equal(await page.getByText('late routing conflict must stay hidden', { exact: true }).count(), 0);
    assert.deepEqual(productionWrites, []);
    assert.deepEqual(errors, []);
  } finally { await browser.close(); await server.close(); }
});

test('upstream create groups configuration and retains advanced and secret drafts across locale and method changes', { timeout: 60_000 }, async () => {
  const root = fileURLToPath(new URL('..', import.meta.url));
  const server = await createServer({ root, configFile: false, plugins: [fixtureAssets()], logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const origin = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 390, height: 1000 } });
    const errors: string[] = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.route('**/*', route => {
      const url = new URL(route.request().url());
      return url.origin === origin && !url.pathname.startsWith('/internal/') ? route.continue() : route.abort();
    });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'zh-CN'));
    await page.goto(`${origin}/e2e/fixtures/form-journey.html?workflows&provider-workflow&provider-create-shape`);
    const workspace = page.locator('.create-journey');
    await workspace.locator('[data-workspace-toggle]').click();
    const advanced = workspace.getByRole('button', { name: '高级网络与用量设置', exact: true });
    const capabilities = workspace.getByRole('button', { name: '协议、图片与视频设置', exact: true });
    const headers = workspace.getByRole('button', { name: '自定义认证请求头', exact: true });
    for (const disclosure of [advanced, capabilities, headers]) assert.equal(await disclosure.getAttribute('aria-expanded'), 'false');
    assert.equal(await workspace.getByLabel('Vendor setup').isVisible(), true);
    assert.equal(await workspace.locator('#root_config_stream_usage_contract').isVisible(), true);
    assert.equal(await workspace.locator('#root_credential_proxy_network_scope').isVisible(), false);
    assert.equal(await workspace.locator('#root_config_timeout_seconds').isVisible(), false);
    assert.equal(await workspace.locator('#root_config_responses_transport').isVisible(), false);
    await workspace.locator('#root_name').fill('Preserved upstream');
    await workspace.locator('#root_config_base_url').fill('https://fixture.invalid/v1');
    await workspace.locator('#root_config_vendor_required').fill('vendor-value');
    await workspace.locator('#root_credential_value').fill('fixture-only-api-secret');
    await workspace.locator('#root_credential_proxy_url').fill('socks5h://10.0.0.20:1080');
    await advanced.click();
    await workspace.locator('#root_config_timeout_seconds').fill('75');
    await workspace.locator('#root_config_reservation_token_bounds_model-x').fill('72000');
    await advanced.click();
    await capabilities.click();
    await workspace.locator('#root_config_responses_transport').selectOption('chat_completions');
    await capabilities.click();
    await headers.click();
    await workspace.locator('#root_credential_header').fill('x-api-key');
    await workspace.locator('#root_credential_prefix').fill('Token ');
    await headers.click();
    await page.locator('[data-change-locale]').click();
    assert.equal(await workspace.locator('#root_name').inputValue(), 'Preserved upstream');
    assert.equal(await workspace.locator('#root_credential_value').inputValue(), 'fixture-only-api-secret');
    await workspace.getByRole('button', { name: 'Account authorization', exact: true }).click();
    await workspace.getByRole('button', { name: 'API credential', exact: true }).click();
    assert.equal(await workspace.locator('#root_credential_value').inputValue(), 'fixture-only-api-secret');
    await workspace.getByRole('button', { name: 'Advanced network and usage settings', exact: true }).click();
    assert.equal(await workspace.locator('#root_config_timeout_seconds').inputValue(), '75');
    assert.equal(await workspace.locator('#root_config_reservation_token_bounds_model-x').inputValue(), '72000');
    await workspace.getByRole('button', { name: 'Advanced network and usage settings', exact: true }).click();
    await workspace.getByRole('button', { name: 'Custom authentication headers', exact: true }).click();
    assert.equal(await workspace.locator('#root_credential_header').inputValue(), 'x-api-key');
    assert.equal(await workspace.locator('#root_credential_prefix').inputValue(), 'Token ');
    await workspace.getByRole('button', { name: 'Custom authentication headers', exact: true }).click();
    await page.evaluate(() => { window.failNextFormWrite = true; });
    const submit = workspace.getByRole('button', { name: 'Add upstream', exact: true });
    await submit.click();
    await workspace.getByRole('alert').waitFor();
    assert.equal(await workspace.locator('#root_credential_value').inputValue(), 'fixture-only-api-secret');
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
    await submit.click();
    await page.waitForFunction(() => document.querySelector('.create-journey')?.getAttribute('data-open') === 'false');
    assert.deepEqual(await page.evaluate(() => window.formJourneyLastProviderCreate), {
      name: 'Preserved upstream', driver: 'http-json', tenant_external_id: 'fixture',
      config: { base_url: 'https://fixture.invalid/v1', vendor_required: 'vendor-value', network_scope: 'public', timeout_seconds: 75,
        reservation_token_bounds: { 'model-x': 72000 }, responses_transport: 'chat_completions', stream_usage_contract: 'none', provider_asset_reads_repeatable: false },
      credential: { type: 'api_key_proxy', value: 'fixture-only-api-secret', proxy_url: 'socks5h://10.0.0.20:1080', proxy_network_scope: 'private', header: 'x-api-key', prefix: 'Token ' },
    });
    await workspace.locator('[data-workspace-toggle]').click();
    assert.equal(await workspace.locator('#root_credential_value').inputValue(), '');
    assert.equal(await page.evaluate(() => window.formJourneyWrites), 2);
    assert.deepEqual(errors, []);
  } finally { await browser.close(); await server.close(); }
});

test('catalog API-key creation preserves empty prefixes, locks submits and separates saved-list recovery from creation', { timeout: 60_000 }, async () => {
  const root = fileURLToPath(new URL('..', import.meta.url));
  const server = await createServer({ root, configFile: false, plugins: [fixtureAssets()], logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const origin = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    const errors: string[] = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.route('**/*', route => {
      const url = new URL(route.request().url());
      return url.origin === origin && !url.pathname.startsWith('/internal/') ? route.continue() : route.abort();
    });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    const fixture = `${origin}/e2e/fixtures/form-journey.html?workflows&provider-workflow&provider-builtin-api-key`;
    const workspace = page.locator('.create-journey');
    async function fillDraft(name: string) {
      await workspace.locator('[data-workspace-toggle]').click();
      await workspace.locator('#root_name').fill(name);
      await workspace.locator('#root_config_base_url').fill('https://fixture.invalid/v1');
      const credentialBranch = workspace.locator('select').filter({ has: page.locator('option', { hasText: /^API key$/ }) });
      assert.equal(await workspace.locator('#root_credential_value').isVisible(), true, 'API key is the default branch');
      await credentialBranch.selectOption({ label: 'No authentication' });
      await workspace.locator('#root_credential_value').waitFor({ state: 'detached' });
      await credentialBranch.selectOption({ label: 'API key' });
      await workspace.locator('#root_credential_value').fill('fixture-only-api-secret');
      await workspace.getByRole('button', { name: 'Custom authentication headers', exact: true }).click();
      await workspace.locator('#root_credential_header').fill('x-api-key');
      await workspace.locator('#root_credential_prefix').fill('');
    }
    await page.goto(fixture);
    await fillDraft('Saved API connection');
    const submit = workspace.locator('button[type="submit"]');
    await page.evaluate(() => { window.deferNextFormProviderCreate = true; });
    await submit.evaluate(button => { (button as HTMLButtonElement).click(); (button as HTMLButtonElement).click(); });
    await page.waitForFunction(() => window.formJourneyWrites === 1);
    assert.equal(await submit.isEnabled(), false);
    assert.equal(await workspace.getByRole('button', { name: 'Account authorization', exact: true }).isEnabled(), false);
    assert.equal(await workspace.getByRole('combobox', { name: 'Service provider', exact: true }).isEnabled(), false);
    assert.equal(await workspace.locator('[data-workspace-toggle]').isEnabled(), false);
    await page.evaluate(() => window.releaseFormProviderCreate(400));
    await workspace.getByRole('alert').waitFor();
    assert.equal(await workspace.locator('#root_name').inputValue(), 'Saved API connection');
    assert.equal(await workspace.locator('#root_credential_value').inputValue(), 'fixture-only-api-secret');
    await page.evaluate(() => { window.failNextFormAccountRead = true; });
    await submit.click();
    await page.waitForFunction(() => document.querySelector('.create-journey')?.getAttribute('data-open') === 'false');
    await page.getByRole('button', { name: 'Reload account list', exact: true }).waitFor();
    await page.getByText('Saved upstream connection Saved API connection.', { exact: true }).waitFor();
    assert.deepEqual(await page.evaluate(() => window.formJourneyLastProviderCreate?.credential), {
      type: 'api_key', value: 'fixture-only-api-secret', header: 'x-api-key', prefix: '',
    });
    await page.getByRole('button', { name: 'Reload account list', exact: true }).click();
    await page.getByRole('button', { name: 'Reload account list', exact: true }).waitFor({ state: 'detached' });
    assert.equal(await page.evaluate(() => window.formJourneyWrites), 2);
    await workspace.locator('[data-workspace-toggle]').click();
    assert.equal(await workspace.locator('#root_name').inputValue(), '');
    for (const field of ['token', 'tenant', 'writeTenant'] as const) for (const status of [201, 400]) {
      await page.goto(fixture);
      await fillDraft(`Old ${field}`);
      await page.evaluate(() => { window.deferNextFormProviderCreate = true; });
      await submit.click();
      await page.waitForFunction(() => window.formJourneyWrites === 1);
      await page.evaluate(field => window.changeFormProviderScope(field), field);
      await page.waitForFunction(() => document.querySelector('.create-journey')?.getAttribute('data-open') === 'false');
      await workspace.locator('[data-workspace-toggle]').click();
      await workspace.locator('#root_name').fill(`New ${field}`);
      await page.evaluate(status => window.releaseFormProviderCreate(status), status);
      await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
      assert.equal(await workspace.getAttribute('data-open'), 'true');
      assert.equal(await workspace.locator('#root_name').inputValue(), `New ${field}`);
      assert.equal(await page.locator('.notice.success').count(), 0);
      assert.equal(await page.getByText('Late create failure', { exact: true }).count(), 0);
      assert.equal(await page.evaluate(() => window.formJourneyWrites), 1);
    }
    assert.deepEqual(errors, []);
  } finally { await browser.close(); await server.close(); }
});
