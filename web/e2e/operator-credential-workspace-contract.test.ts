import assert from 'node:assert/strict';
import { existsSync, mkdirSync } from 'node:fs';
import { readdir } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

import { chromium } from 'playwright';
import { createServer } from 'vite';

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

async function calls(page: import('playwright').Page) {
  return page.evaluate(() => window.credentialFixture.calls);
}

async function nextPaint(page: import('playwright').Page) {
  await page.evaluate(() => new Promise<void>((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
}

test('credential workspaces isolate loads and preserve one-time service plaintext', { timeout: 60_000 }, async () => {
  const executablePath = await localChromiumExecutable();
  if (!executablePath) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required for the credential workspace CI gate');
    return test.skip('a local Chromium runtime is required for credential workspace behavior assertions');
  }
  const server = await createServer({ root: webRoot, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0, strictPort: false } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ executablePath, headless: true });
  const fixture = (scenario: string) => `http://127.0.0.1:${address.port}/e2e/fixtures/operator-credential-workspace.html?scenario=${scenario}`;
  try {
    const allTenants = await browser.newPage();
    await allTenants.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await allTenants.goto(fixture('all-tenants'));
    await allTenants.getByText('All tenant client', { exact: true }).waitFor();
    await allTenants.getByText('Tenant: tenant-visible', { exact: false }).waitFor();
    assert.equal(await allTenants.getByRole('button', { name: 'Rename', exact: true }).isDisabled(), true);
    const limits = allTenants.getByRole('button', { name: 'Current limit state', exact: true });
    assert.equal(await limits.isDisabled(), false, 'stable key ID permits a read-only limit lookup without selecting a tenant');
    await limits.click();
    await allTenants.waitForFunction(() => window.credentialFixture.calls.some((call) => call.endsWith('/keys/key-all/limits')));
    assert.equal((await calls(allTenants)).some((call) => call.startsWith('/internal/v1/model-routes')), false, 'all-tenant key review must not fetch route editors');

    const routeFailure = await browser.newPage();
    await routeFailure.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await routeFailure.goto(fixture('route-failure'));
    await routeFailure.getByText('Route failure client', { exact: true }).waitFor();
    await routeFailure.getByRole('alert').getByText('route catalog unavailable', { exact: true }).waitFor();
    assert.equal(await routeFailure.getByText('Route failure client', { exact: true }).count(), 1, 'a route error cannot hide a successfully loaded key page');

    const recovery = await browser.newPage();
    await recovery.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await recovery.goto(fixture('client-recovery'));
    await recovery.getByText('Recoverable client', { exact: true }).waitFor();
    await recovery.getByRole('button', { name: 'Recover and copy credential', exact: true }).click();
    await recovery.getByRole('button', { name: 'Confirm and continue', exact: true }).click();
    await recovery.getByText('mts_client_recovered', { exact: true }).waitFor();
    const recoveryRequest = await recovery.evaluate(() => window.credentialFixture.requests.find((request) => request.path.endsWith('/credential-recovery/copy')));
    assert.deepEqual(recoveryRequest, {
      method: 'POST',
      path: '/internal/v1/keys/key-recovery/credential-recovery/copy',
      cache: 'no-store',
      credentials: 'omit',
      referrerPolicy: 'no-referrer',
      hasSignal: true,
    });

    const race = await browser.newPage();
    await race.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await race.goto(fixture('scope-race'));
    await race.waitForFunction(() => window.credentialFixture.calls.some((call) => call.includes('tenant_external_id=tenant-a')));
    await race.getByRole('button', { name: 'Switch tenant', exact: true }).click();
    await race.getByText('Scope B client', { exact: true }).waitFor();
    await race.evaluate(() => window.credentialFixture.releaseCredentialScopeA());
    await nextPaint(race);
    assert.equal(await race.getByText('Scope A client', { exact: true }).count(), 0, 'a late aborted scope cannot overwrite the replacement scope or release its request identity');

    const lock = await browser.newPage();
    await lock.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await lock.goto(fixture('scope-lock'));
    await lock.waitForFunction(() => window.credentialFixture.calls.some((call) => call.includes('tenant_external_id=tenant-a')));
    await lock.getByRole('button', { name: 'Switch tenant', exact: true }).click();
    await lock.getByText('Scope B 101', { exact: true }).waitFor();
    const loadMore = lock.locator('.load-more button');
    await loadMore.click();
    await lock.waitForFunction(() => window.credentialFixture.calls.some((call) => call.startsWith('/internal/v1/keys?') && call.includes('before_id=')));
    await lock.evaluate(() => window.credentialFixture.releaseCredentialScopeA());
    await nextPaint(lock);
    assert.equal(await loadMore.isDisabled(), true, 'the stale scope cannot release the active cursor request identity');
    assert.equal(await lock.getByText('Scope A client', { exact: true }).count(), 0, 'the old scope remains invisible while a replacement cursor page is pending');
    await lock.evaluate(() => window.credentialFixture.releaseCredentialCursor());
    await lock.getByText('Scope B older client', { exact: true }).waitFor();
    const cursorCalls = (await calls(lock)).filter((call) => call.startsWith('/internal/v1/keys?') && call.includes('before_id='));
    assert.equal(cursorCalls.length, 1, 'the stale scope cannot release the active cursor request for a second load');
    await Promise.all([allTenants.close(), routeFailure.close(), recovery.close(), race.close(), lock.close()]);

    const plaintext = await browser.newPage();
    plaintext.setDefaultTimeout(10_000);
    await plaintext.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await plaintext.goto(fixture('service-plaintext'));
    await plaintext.getByText('Existing service credential', { exact: true }).waitFor();
    await plaintext.locator('details.create-resource > summary').click();
    const create = plaintext.getByRole('button', { name: 'Create service credential', exact: true });
    await create.evaluate((button) => {
      (button as HTMLButtonElement).click();
      (button as HTMLButtonElement).click();
    });
    await plaintext.waitForFunction(() => window.credentialFixture.requests.filter((request) => request.method === 'POST' && request.path === '/internal/v1/service-tokens').length === 1);
    await plaintext.evaluate(() => window.credentialFixture.releaseIssue('mts_service_secret_first'));
    await plaintext.getByText('mts_service_secret_first', { exact: true }).waitFor();
    const oneTimePanel = plaintext.locator('aside.one-time');
    assert.equal(await oneTimePanel.getAttribute('role'), null, 'rendering plaintext must not announce it as a live region');
    assert.equal(await oneTimePanel.locator('code').evaluate((element) => element.closest('[role="status"], [aria-live]') === null), true);
    await plaintext.getByRole('button', { name: 'Copy credential', exact: true }).click();
    await plaintext.getByRole('alert').getByText('Copy failed. Use download or select the credential above manually.', { exact: true }).waitFor();
    assert.equal(await plaintext.locator('textarea').count(), 0, 'a throwing clipboard fallback clears and removes its plaintext node');
    await plaintext.getByRole('button', { name: 'Download credential', exact: true }).click();
    await plaintext.waitForFunction(() => window.credentialFixture.createdObjectUrls.length === 1 && window.credentialFixture.revokedObjectUrls[0] === window.credentialFixture.createdObjectUrls[0]);
    assert.equal(await create.isDisabled(), true, 'visible plaintext blocks another service credential issuance');
    assert.equal(await plaintext.getByRole('button', { name: 'Rotate service credential', exact: true }).isDisabled(), true, 'visible plaintext blocks rotation from replacing it');
    plaintext.once('dialog', (dialog) => void dialog.accept());
    await plaintext.getByRole('button', { name: 'Close', exact: true }).click();
    await plaintext.getByText('mts_service_secret_first', { exact: true }).waitFor({ state: 'detached' });
    await plaintext.waitForFunction(() => Array.from(document.querySelectorAll('button')).some((button) => button.textContent === 'Create service credential' && !button.disabled));
    assert.equal(await create.isDisabled(), false, 'confirmed dismissal clears plaintext and releases issuance controls');
    assert.equal(await plaintext.getByRole('button', { name: 'Rotate service credential', exact: true }).isDisabled(), false);

    const aba = await browser.newPage();
    aba.setDefaultTimeout(10_000);
    await aba.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await aba.goto(fixture('service-scope-aba'));
    await aba.getByText('Existing service credential', { exact: true }).waitFor();
    await aba.locator('details.create-resource > summary').click();
    const abaCreate = aba.getByRole('button', { name: 'Create service credential', exact: true });
    await abaCreate.click();
    await aba.waitForFunction(() => window.credentialFixture.requests.filter((request) => request.method === 'POST' && request.path === '/internal/v1/service-tokens').length === 1);
    await aba.getByRole('button', { name: 'Switch tenant', exact: true }).click();
    await aba.getByText('Tenant tenant-b', { exact: true }).waitFor();
    await aba.getByRole('button', { name: 'Switch tenant', exact: true }).click();
    await aba.getByText('Tenant tenant-a', { exact: true }).waitFor();
    await aba.evaluate(() => window.credentialFixture.releaseIssue('mts_service_secret_stale'));
    await nextPaint(aba);
    assert.equal(await aba.getByText('mts_service_secret_stale', { exact: true }).count(), 0, 'an old response cannot reappear after an A-B-A scope transition');
    await aba.waitForFunction(() => Array.from(document.querySelectorAll('button')).some((button) => button.textContent === 'Create service credential' && !button.disabled));
    assert.equal(await abaCreate.isDisabled(), false, 'scope cleanup releases the stale operation guard');
    await abaCreate.click();
    await aba.waitForFunction(() => window.credentialFixture.requests.filter((request) => request.method === 'POST' && request.path === '/internal/v1/service-tokens').length === 2);
    await aba.evaluate(() => window.credentialFixture.releaseIssue('mts_service_secret_current'));
    await aba.getByText('mts_service_secret_current', { exact: true }).waitFor();

    // Exercise the actual page and shipped schemas on this same server. Each
    // locale creates and edits a different mode, covering both payload values
    // in both endpoints without a second synthetic component/server fixture.
    for (const locale of ['en', 'zh-CN'] as const) {
      const client = await browser.newPage({ viewport: { width: 390, height: 844 } });
      client.setDefaultTimeout(5_000);
      await client.addInitScript(value => localStorage.setItem('mtc-locale', value), locale);
      await client.goto(fixture('client-form'));
      const english = locale === 'en';
      const modeLabel = english ? 'Metering and limit mode' : '计量与限额模式';
      await client.getByText('Editable client', { exact: true }).waitFor();
      await client.getByRole('button', { name: english ? 'Policy and limits' : '权限与限流', exact: true }).click();
      const edit = client.locator('.inline-editor.form-panel');
      const editMode = edit.getByRole('combobox', { name: modeLabel, exact: true });
      await edit.locator('#root_max_concurrency').focus();
      await client.keyboard.press('Tab');
      assert.equal(await editMode.evaluate(element => document.activeElement === element), true);
      const editedMode = english ? 'metered_unlimited' : 'prepaid';
      await editMode.selectOption(editedMode);
      await edit.getByRole('button', { name: english ? 'Save' : '保存', exact: true }).click();
      await client.waitForFunction(() => window.credentialFixture.requests.some(request => request.method === 'PUT' && request.path.endsWith('/key-form/policy')));
      const policyRequest = await client.evaluate(() => window.credentialFixture.requests.find(request => request.method === 'PUT' && request.path.endsWith('/key-form/policy'))!);
      assert.equal(JSON.parse(policyRequest.body!).enforcement_mode, editedMode);

      const create = client.locator('details.create-resource');
      await create.locator(':scope > summary').click();
      const routes = create.getByRole('combobox', { name: english ? 'Specific routes' : '具体路由', exact: true });
      const groups = create.getByRole('combobox', { name: english ? 'Route groups' : '路由组', exact: true });
      await routes.fill('Research model');
      await routes.press('ArrowDown');
      await routes.press('Enter');
      await routes.press('Escape');
      await groups.fill('Research group');
      await groups.press('ArrowDown');
      await groups.press('Enter');
      await groups.press('Escape');
      assert.equal(await create.locator('.schema-array').count(), 0, 'routing IDs have one named control each');
      assert.equal(await create.getByText('Unsupported field schema', { exact: false }).count(), 0);
      await create.locator('#root_principal_external_id').fill('fixture-principal');
      await create.locator('#root_alias').fill('Created client');
      const createMode = create.getByRole('combobox', { name: modeLabel, exact: true });
      await create.locator('#root_policy_max_concurrency').focus();
      await client.keyboard.press('Tab');
      assert.equal(await createMode.evaluate(element => document.activeElement === element), true);
      await client.keyboard.press('Shift+Tab');
      assert.equal(await create.locator('#root_policy_max_concurrency').evaluate(element => document.activeElement === element), true);
      const createdMode = english ? 'prepaid' : 'metered_unlimited';
      await createMode.selectOption(createdMode);
      for (const theme of ['light', 'dark']) {
        await client.evaluate(value => { document.documentElement.dataset.theme = value; }, theme);
        assert.equal(await client.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
        assert.equal(await createMode.evaluate(element => getComputedStyle(element).fontSize), '16px');
      }
      const artifacts = fileURLToPath(new URL('../e2e-artifacts/upstream-availability', import.meta.url));
      mkdirSync(artifacts, { recursive: true });
      await create.screenshot({ path: `${artifacts}/credential-workspace-${locale}-mobile.png` });
      await create.locator('button[type="submit"]').click();
      await client.getByText('mts_fixture_created', { exact: true }).waitFor();
      const createRequests = await client.evaluate(() => window.credentialFixture.requests.filter(request => request.method === 'POST' && request.path === '/internal/v1/keys'));
      assert.equal(createRequests.length, 1);
      const body = JSON.parse(createRequests[0].body!);
      assert.equal(body.tenant_external_id, 'tenant-a');
      assert.equal(body.principal_external_id, 'fixture-principal');
      assert.equal(body.alias, 'Created client');
      assert.equal(body.policy.enforcement_mode, createdMode);
      assert.deepEqual(body.route_ids, ['00000000-0000-4000-8000-000000000001']);
      assert.deepEqual(body.route_group_ids, ['00000000-0000-4000-8000-000000000002']);
      assert.equal('route_ids' in body.policy || 'route_group_ids' in body.policy, false);
      await client.close();
    }
  } finally {
    await browser.close();
    await server.close();
  }
});
