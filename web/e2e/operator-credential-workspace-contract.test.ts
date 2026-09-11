import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
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
      requests: Array<{ method: string; path: string }>;
      releaseIssue: (token: string) => void;
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

    const race = await browser.newPage();
    await race.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await race.goto(fixture('scope-race'));
    await race.waitForFunction(() => window.credentialFixture.calls.some((call) => call.includes('tenant_external_id=tenant-a')));
    await race.getByRole('button', { name: 'Switch tenant', exact: true }).click();
    await race.getByText('Scope B client', { exact: true }).waitFor();
    await race.waitForTimeout(260);
    assert.equal(await race.getByText('Scope A client', { exact: true }).count(), 0, 'a late aborted scope cannot overwrite the replacement scope or release its request identity');

    const lock = await browser.newPage();
    await lock.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await lock.goto(fixture('scope-lock'));
    await lock.waitForFunction(() => window.credentialFixture.calls.some((call) => call.includes('tenant_external_id=tenant-a')));
    await lock.getByRole('button', { name: 'Switch tenant', exact: true }).click();
    await lock.getByText('Scope B 101', { exact: true }).waitFor();
    await lock.getByRole('button', { name: 'Load more credentials', exact: true }).click();
    await lock.getByText('Scope B older client', { exact: true }).waitFor();
    const cursorCalls = (await calls(lock)).filter((call) => call.startsWith('/internal/v1/keys?') && call.includes('before_id='));
    assert.equal(cursorCalls.length, 1, 'the stale scope cannot release the active cursor request for a second load');
    assert.equal(await lock.getByText('Scope A client', { exact: true }).count(), 0, 'the old scope remains invisible while a replacement cursor page is pending');
    await Promise.all([allTenants.close(), routeFailure.close(), race.close(), lock.close()]);

    const plaintext = await browser.newPage();
    plaintext.setDefaultTimeout(10_000);
    await plaintext.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await plaintext.goto(fixture('service-plaintext'));
    await plaintext.getByText('Existing service credential', { exact: true }).waitFor();
    const create = plaintext.getByRole('button', { name: 'Create service credential', exact: true });
    await create.evaluate((button) => {
      (button as HTMLButtonElement).click();
      (button as HTMLButtonElement).click();
    });
    await plaintext.waitForFunction(() => window.credentialFixture.requests.filter((request) => request.method === 'POST' && request.path === '/internal/v1/service-tokens').length === 1);
    await plaintext.evaluate(() => window.credentialFixture.releaseIssue('mts_service_secret_first'));
    await plaintext.getByText('mts_service_secret_first', { exact: true }).waitFor();
    assert.equal(await create.isDisabled(), true, 'visible plaintext blocks another service credential issuance');
    assert.equal(await plaintext.getByRole('button', { name: 'Rotate', exact: true }).isDisabled(), true, 'visible plaintext blocks rotation from replacing it');
    plaintext.once('dialog', (dialog) => void dialog.accept());
    await plaintext.getByRole('button', { name: 'Close', exact: true }).click();
    await plaintext.getByText('mts_service_secret_first', { exact: true }).waitFor({ state: 'detached' });
    await plaintext.waitForFunction(() => Array.from(document.querySelectorAll('button')).some((button) => button.textContent === 'Create service credential' && !button.disabled));
    assert.equal(await create.isDisabled(), false, 'confirmed dismissal clears plaintext and releases issuance controls');
    assert.equal(await plaintext.getByRole('button', { name: 'Rotate', exact: true }).isDisabled(), false);

    const aba = await browser.newPage();
    aba.setDefaultTimeout(10_000);
    await aba.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await aba.goto(fixture('service-scope-aba'));
    await aba.getByText('Existing service credential', { exact: true }).waitFor();
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
  } finally {
    await browser.close();
    await server.close();
  }
});
