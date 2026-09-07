import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { readdir } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

import { chromium, type Page } from 'playwright';
import { createServer } from 'vite';

const webRoot = fileURLToPath(new URL('..', import.meta.url));
const screenshotWidths = [320, 390, 768, 1024, 1440, 1920, 2560] as const;

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

function fixture(port: number, scenario: 'default' | 'multiple' | 'denied', view = 'tenants') {
  return `http://127.0.0.1:${port}/e2e/fixtures/tenant-management.html?scenario=${scenario}&view=${view}`;
}

async function fixtureCalls(page: Page) {
  return page.evaluate(() => window.tenantFixture.calls);
}

test('tenant management has one sidebar route and single-default scope stays implicit through refresh and history', { timeout: 30_000 }, async () => {
  const executablePath = await localChromiumExecutable();
  if (!executablePath) return test.skip('a local Chromium runtime is required for tenant navigation assertions');
  const server = await createServer({ root: webRoot, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0, strictPort: false } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ executablePath, headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(fixture(address.port, 'default', 'overview'));
    const tenantLink = page.getByRole('link', { name: 'Tenant management', exact: true });
    await tenantLink.click();
    await page.getByRole('heading', { name: 'Tenant management', exact: true }).waitFor();
    assert.equal(await page.getByRole('link', { name: 'Tenant management', exact: true }).count(), 1);
    assert.equal(await tenantLink.getAttribute('aria-current'), 'page');
    assert.equal(await page.locator('.tenant-scope-switcher').count(), 0, 'a sole default tenant must not render scope controls');
    assert.equal(await page.locator('.console-context').count(), 0, 'a sole default tenant must not render a scope card');
    assert.ok((await fixtureCalls(page)).some((call) => call === 'GET /internal/v1/tenant-management'));
    assert.match(page.url(), /view=tenants/);

    await page.reload();
    await page.getByRole('heading', { name: 'Tenant management', exact: true }).waitFor();
    const lifecycleCallsBeforeBack = (await fixtureCalls(page)).filter((call) => call === 'GET /internal/v1/tenant-management').length;
    await page.goBack();
    await page.waitForFunction(() => new URL(location.href).searchParams.get('view') === 'overview');
    assert.equal((await fixtureCalls(page)).filter((call) => call === 'GET /internal/v1/tenant-management').length, lifecycleCallsBeforeBack, 'navigating back must not mount tenant CRUD');
  } finally {
    await browser.close();
    await server.close();
  }
});

test('multi-tenant scope exposes tenant CRUD, dependency refusal, authorization boundaries, and screenshot widths', { timeout: 30_000 }, async () => {
  const executablePath = await localChromiumExecutable();
  if (!executablePath) return test.skip('a local Chromium runtime is required for tenant lifecycle assertions');
  const server = await createServer({ root: webRoot, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0, strictPort: false } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ executablePath, headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(fixture(address.port, 'multiple'));
    await page.getByRole('heading', { name: 'Tenant management', exact: true }).waitFor();
    const scope = page.getByRole('combobox', { name: 'Tenant scope', exact: true });
    await scope.selectOption('north');
    assert.equal(await scope.inputValue(), 'north');

    await page.getByLabel('Tenant ID', { exact: true }).fill('west');
    await page.getByRole('button', { name: 'Add tenant', exact: true }).click();
    await page.getByText('west', { exact: true }).waitFor();

    let north = page.locator('.managed-resource').filter({ has: page.getByText('north', { exact: true }) });
    await north.getByRole('button', { name: 'Rename', exact: true }).click();
    const renameDialog = page.getByRole('dialog', { name: 'Rename tenant', exact: true });
    await renameDialog.getByLabel('Tenant ID', { exact: true }).fill('north-renamed');
    await renameDialog.getByRole('button', { name: 'Rename', exact: true }).click();
    await page.getByText('north-renamed', { exact: true }).waitFor();
    north = page.locator('.managed-resource').filter({ has: page.getByText('north-renamed', { exact: true }) });

    await north.getByRole('button', { name: 'Archive', exact: true }).click();
    const archiveDialog = page.getByRole('dialog', { name: 'Archive tenant', exact: true });
    await archiveDialog.waitFor();
    await page.keyboard.press('Escape');
    assert.equal(await page.getByRole('dialog', { name: 'Archive tenant', exact: true }).count(), 0, 'Escape must dismiss the tenant action dialog');
    await north.getByRole('button', { name: 'Archive', exact: true }).click();
    await page.getByRole('dialog', { name: 'Archive tenant', exact: true }).getByRole('button', { name: 'Archive', exact: true }).click();
    await north.getByRole('button', { name: 'Restore', exact: true }).waitFor();
    await north.getByRole('button', { name: 'Restore', exact: true }).click();
    await page.getByRole('dialog', { name: 'Restore tenant', exact: true }).getByRole('button', { name: 'Restore', exact: true }).click();
    await north.getByRole('button', { name: 'Archive', exact: true }).waitFor();
    await north.getByRole('button', { name: 'Archive', exact: true }).click();
    await page.getByRole('dialog', { name: 'Archive tenant', exact: true }).getByRole('button', { name: 'Archive', exact: true }).click();
    await north.getByRole('button', { name: 'Delete', exact: true }).click();
    await page.getByRole('dialog', { name: 'Delete tenant', exact: true }).getByRole('button', { name: 'Delete', exact: true }).click();
    await page.getByRole('alert').getByText('Tenant still owns resources', { exact: true }).waitFor();

    const calls = await fixtureCalls(page);
    for (const method of ['POST /internal/v1/tenant-management', 'PATCH /internal/v1/tenant-management/north', 'POST /internal/v1/tenant-management/north-renamed/archive', 'POST /internal/v1/tenant-management/north-renamed/restore', 'DELETE /internal/v1/tenant-management/north-renamed']) {
      assert.ok(calls.some((call) => call === method), `missing tenant lifecycle request: ${method}`);
    }

    for (const width of screenshotWidths) {
      await page.setViewportSize({ width, height: 900 });
      const layout = await page.evaluate(() => ({ document: document.documentElement.clientWidth, scroll: document.documentElement.scrollWidth }));
      assert.ok(layout.scroll <= layout.document, `${width}px tenant fixture must not overflow`);
      assert.ok((await page.screenshot()).byteLength > 0, `${width}px tenant screenshot must render`);
    }

    const denied = await browser.newPage({ viewport: { width: 1024, height: 720 } });
    await denied.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await denied.goto(fixture(address.port, 'denied'));
    await denied.getByRole('alert').getByText('Tenant access denied', { exact: true }).waitFor();
    assert.equal(await denied.getByRole('button', { name: 'Connect', exact: true }).isDisabled(), true);
    assert.equal((await fixtureCalls(denied)).some((call) => call.includes('/internal/v1/tenant-management')), false, 'an unauthorized credential cannot load tenant CRUD');
  } finally {
    await browser.close();
    await server.close();
  }
});
