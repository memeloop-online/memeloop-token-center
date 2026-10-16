import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { readdir } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

import { chromium, type Page } from 'playwright';
import { createIsolatedFixtureServer as createServer } from './support/isolated-vite-server.js';
import { operatorFixturePlugin } from './support/navigation-fixture-server.js';

declare global {
  interface Window {
    degradedFixture: { calls: string[]; mode: 'ok' | 'outage' | 'forbidden' | 'management-denied' };
  }
}

const webRoot = fileURLToPath(new URL('..', import.meta.url));
const credentialA = 'mts_degraded_fixture_alpha';
const credentialB = 'mts_degraded_fixture_beta';
const storageKey = 'mtc.operator.service-credential.v1';

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

async function requireChromium() {
  const executablePath = await localChromiumExecutable();
  if (!executablePath) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required for the degraded-access browser contract');
    test.skip('a local Chromium runtime is required for degraded-access browser assertions');
    return undefined;
  }
  return executablePath;
}

async function openFixture(executablePath: string) {
  const server = await createServer({ root: webRoot, configFile: false, logLevel: 'silent', plugins: [operatorFixturePlugin('/e2e/fixtures/operator-degraded-access.html')], server: { host: '127.0.0.1', port: 0, strictPort: false } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ executablePath, headless: true });
  const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
  await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
  return { server, browser, page, port: address.port };
}

function fixtureUrl(port: number, view: string, seeded = true) {
  return `http://127.0.0.1:${port}/operator?view=${view}${seeded ? '' : '&seeded=0'}`;
}

async function setMode(page: Page, mode: Window['degradedFixture']['mode']) {
  await page.evaluate((next) => { window.degradedFixture.mode = next; }, mode);
}

async function fixtureCalls(page: Page) {
  return page.evaluate(() => window.degradedFixture.calls);
}

function savedCredential(page: Page) {
  return page.evaluate((key) => localStorage.getItem(key), storageKey);
}

function tenantRow(page: Page, tenantId: string) {
  return page.locator('.tenant-manager .managed-resource').filter({
    has: page.locator('.managed-resource-header b', { hasText: new RegExp(`^${tenantId}$`) }),
  });
}

async function archiveTenant(page: Page, tenantId: string) {
  await tenantRow(page, tenantId).getByRole('button', { name: 'Archive', exact: true }).click();
  await page.getByRole('dialog').getByRole('button', { name: 'Archive', exact: true }).click();
}

async function openTenantManager(page: Page, port: number, seeded = true) {
  await page.goto(fixtureUrl(port, 'tenants', seeded));
  await page.getByRole('heading', { name: 'Tenant management', exact: true }).waitFor();
  await page.locator('.tenant-list').waitFor();
}

test('outage retry revalidates in place and keeps the mounted page and its unsubmitted draft', { timeout: 60_000 }, async () => {
  const executablePath = await requireChromium();
  if (!executablePath) return;
  const { server, browser, page, port } = await openFixture(executablePath);
  try {
    await openTenantManager(page, port);
    await page.getByLabel('Tenant ID', { exact: true }).fill('draft-keep');
    await setMode(page, 'outage');
    await archiveTenant(page, 'north');
    const fault = page.getByRole('alert').filter({ hasText: 'temporarily unavailable' });
    await fault.waitFor();
    await fault.getByRole('button', { name: 'Retry', exact: true }).waitFor();
    assert.equal(await page.getByLabel('Tenant ID', { exact: true }).inputValue(), 'draft-keep', 'the outage must keep the mounted form and its draft');
    assert.equal((await fixtureCalls(page)).filter((call) => call.startsWith('GET /internal/v1/tenant-management')).length, 2, 'mount and archive each read the management list once');
    await setMode(page, 'ok');
    await fault.getByRole('button', { name: 'Retry', exact: true }).click();
    await fault.waitFor({ state: 'detached' });
    assert.equal(await page.getByLabel('Tenant ID', { exact: true }).inputValue(), 'draft-keep', 'retry must revalidate the session in place instead of remounting the page');
    assert.equal((await fixtureCalls(page)).filter((call) => call.startsWith('GET /internal/v1/tenant-management')).length, 2, 'a successful retry must not remount the tenant page');
    assert.equal((await fixtureCalls(page)).filter((call) => call === 'GET /internal/v1/tenants [alpha]').length, 3, 'mount, failed refresh and retry each hit the baseline read once');
  } finally {
    await browser.close();
    await server.close();
  }
});

test('tenant selection keeps the fault and its retry until a read succeeds', { timeout: 60_000 }, async () => {
  const executablePath = await requireChromium();
  if (!executablePath) return;
  const { server, browser, page, port } = await openFixture(executablePath);
  try {
    await openTenantManager(page, port);
    await setMode(page, 'outage');
    await page.getByLabel('Tenant ID', { exact: true }).fill('east');
    await page.getByRole('button', { name: 'Add tenant', exact: true }).click();
    const fault = page.getByRole('alert').filter({ hasText: 'temporarily unavailable' });
    await fault.waitFor();
    await page.locator('.tenant-picker select').selectOption('north');
    assert.equal(await page.locator('.tenant-picker select').inputValue(), 'north');
    assert.equal(await page.getByRole('alert').filter({ hasText: 'temporarily unavailable' }).count(), 1, 'switching tenants must not erase the fault');
    await fault.getByRole('button', { name: 'Retry', exact: true }).waitFor();
    await setMode(page, 'ok');
    await page.getByLabel('Tenant ID', { exact: true }).fill('west');
    await page.getByRole('button', { name: 'Add tenant', exact: true }).click();
    await fault.waitFor({ state: 'detached' });
    assert.equal(await page.locator('.tenant-picker select').inputValue(), 'north', 'the explicit scope choice survives the recovery read');
    assert.equal((await fixtureCalls(page)).filter((call) => call === 'POST /internal/v1/tenant-management [alpha]').length, 2);
  } finally {
    await browser.close();
    await server.close();
  }
});

test('a rejected replacement candidate never disturbs the validated session', { timeout: 60_000 }, async () => {
  const executablePath = await requireChromium();
  if (!executablePath) return;
  const { server, browser, page, port } = await openFixture(executablePath);
  try {
    await page.goto(fixtureUrl(port, 'system-settings'));
    await page.getByRole('heading', { name: 'Management access', exact: true }).waitFor();
    await page.locator('.settings-access-card .status.ok').waitFor();
    await page.getByLabel('Service credential', { exact: true }).fill(credentialB);
    await page.getByRole('button', { name: 'Replace credential', exact: true }).click();
    await page.getByRole('alert').filter({ hasText: 'Credential rejected' }).waitFor();
    assert.equal(await savedCredential(page), credentialA, 'a failed candidate must not evict the validated saved credential');
    await page.locator('.settings-access-card .status.ok').waitFor();
    assert.equal((await fixtureCalls(page)).filter((call) => call.includes('[_beta]')).length, 1, 'the candidate is tried exactly once and never reused');
    await openTenantManager(page, port);
    assert.ok((await fixtureCalls(page)).some((call) => call === 'GET /internal/v1/tenant-management [alpha]'), 'the validated identity keeps authorizing management reads');
  } finally {
    await browser.close();
    await server.close();
  }
});

test('a baseline 403 deauthorizes the console while a scoped API 403 stays local', { timeout: 60_000 }, async () => {
  const executablePath = await requireChromium();
  if (!executablePath) return;
  const { server, browser, page, port } = await openFixture(executablePath);
  try {
    await openTenantManager(page, port);
    await setMode(page, 'forbidden');
    await archiveTenant(page, 'north');
    const denial = page.getByRole('alert').filter({ hasText: 'Operator role revoked' });
    await denial.waitFor();
    assert.equal(await denial.getByRole('button', { name: 'Retry' }).count(), 0, 'a definitive denial must not pose as a retryable outage');
    await page.getByRole('heading', { name: 'Management access', exact: true }).waitFor();
    assert.equal(await page.locator('.tenant-manager').count(), 0, 'a denied identity no longer authorizes management pages');
    assert.equal(await page.locator('.tenant-picker select').count(), 0, 'scope controls hide once authorization is gone');
    assert.equal(await savedCredential(page), credentialA, 'a denial keeps the saved credential for inspection or explicit clear');
    assert.equal((await fixtureCalls(page)).filter((call) => call === 'GET /internal/v1/tenants [alpha]').length, 2, 'the denial answers once without an outage retry loop');
    assert.equal((await fixtureCalls(page)).filter((call) => call.startsWith('GET /internal/v1/tenant-management')).length, 2, 'no management read runs after deauthorization');

    await openTenantManager(page, port);
    await setMode(page, 'management-denied');
    await page.getByLabel('Tenant ID', { exact: true }).fill('east');
    await page.getByRole('button', { name: 'Add tenant', exact: true }).click();
    await page.locator('.tenant-manager').getByRole('alert').filter({ hasText: 'Tenant management scope missing' }).waitFor();
    assert.equal(await page.getByRole('heading', { name: 'Management access' }).count(), 0, 'a scoped API denial must not sign the session out');
    assert.equal(await page.locator('.tenant-picker select').inputValue(), 'default', 'the validated scope survives a narrow API denial');
    assert.equal(await savedCredential(page), credentialA);
  } finally {
    await browser.close();
    await server.close();
  }
});

test('an unproven candidate is never promoted or stored on a definitive rejection', { timeout: 60_000 }, async () => {
  const executablePath = await requireChromium();
  if (!executablePath) return;
  const { server, browser, page, port } = await openFixture(executablePath);
  try {
    await page.goto(fixtureUrl(port, 'tenants', false));
    await page.getByRole('heading', { name: 'Management access', exact: true }).waitFor();
    await setMode(page, 'forbidden');
    await page.getByLabel('Service credential', { exact: true }).fill(credentialA);
    await page.getByRole('button', { name: 'Connect', exact: true }).click();
    await page.getByRole('alert').filter({ hasText: 'Operator role revoked' }).waitFor();
    assert.equal(await page.locator('.tenant-manager').count(), 0, 'a denied candidate never reaches management pages');
    assert.equal(await page.getByRole('button', { name: 'Retry' }).count(), 0, 'a definitive denial offers no outage retry');
    assert.equal(await savedCredential(page), null, 'a denied candidate is never stored');
    await setMode(page, 'ok');
    await page.getByLabel('Service credential', { exact: true }).fill('mts_degraded_fixture_gamma');
    await page.getByRole('button', { name: 'Connect', exact: true }).click();
    await page.getByRole('alert').filter({ hasText: 'Credential rejected' }).waitFor();
    assert.equal(await page.locator('.tenant-manager').count(), 0);
    assert.equal(await savedCredential(page), null, 'a rejected candidate is never stored');
  } finally {
    await browser.close();
    await server.close();
  }
});
