import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { readdir } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

import { chromium, type Page } from 'playwright';
import { createServer } from 'vite';
import { operatorFixturePlugin } from './support/navigation-fixture-server.js';
import { captureNavigationIdentity, seedNavigationIdentity } from './support/navigation-identity-artifacts.js';

declare global {
  interface Window {
    tenantFixture: { calls: string[]; holdNext?: boolean; release?: () => void };
  }
}

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

function fixture(port: number, scenario: 'default' | 'multiple' | 'denied' | 'empty' | 'slow' | 'management-denied' | 'management-error' | 'background-refresh' | 'services-denied' | 'services-empty', view = 'tenants') {
  return `http://127.0.0.1:${port}/operator?scenario=${scenario}&view=${view}`;
}

async function fixtureCalls(page: Page) {
  return page.evaluate(() => window.tenantFixture.calls);
}

function tenantRow(page: Page, tenantId: string) {
  return page.locator('.tenant-manager .managed-resource').filter({
    has: page.locator('.managed-resource-header b', { hasText: new RegExp(`^${tenantId}$`) }),
  });
}

test('tenant management has one sidebar route and single-default scope stays implicit through refresh and history', { timeout: 30_000 }, async () => {
  const executablePath = await localChromiumExecutable();
  if (!executablePath) return test.skip('a local Chromium runtime is required for tenant navigation assertions');
  const server = await createServer({ root: webRoot, configFile: false, logLevel: 'silent', plugins: [operatorFixturePlugin('/e2e/fixtures/tenant-management.html')], server: { host: '127.0.0.1', port: 0, strictPort: false } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ executablePath, headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(fixture(address.port, 'default', 'overview'));
    const tenantLink = page.getByRole('link', { name: 'Identity management', exact: true });
    await tenantLink.click();
    await page.getByRole('heading', { name: 'Tenant management', exact: true }).waitFor();
    assert.equal(await page.getByRole('link', { name: 'Identity management', exact: true }).count(), 1);
    assert.equal(await tenantLink.getAttribute('aria-current'), 'page');
    assert.equal(await page.locator('.tenant-scope-switcher').count(), 0, 'a sole default tenant must not render scope controls');
    assert.equal(await page.locator('.console-context').count(), 0, 'a sole default tenant must not render a scope card');
    assert.ok((await fixtureCalls(page)).some((call) => call === 'GET /internal/v1/tenant-management'));
    assert.match(page.url(), /view=tenants/);
    assert.equal(await page.getByRole('link', { name: 'Service credentials', exact: true }).count(), 0);
    assert.equal(await page.getByRole('tab', { name: 'Tenants', exact: true }).getAttribute('aria-selected'), 'true');
    await page.getByLabel('Tenant ID', { exact: true }).fill('keep-this-draft');
    await page.getByRole('tab', { name: 'Service credentials', exact: true }).click();
    await page.getByRole('dialog').waitFor();
    assert.match(page.url(), /view=tenants/);
    assert.equal(await page.getByRole('tab', { name: 'Tenants', exact: true }).getAttribute('aria-selected'), 'true');
    assert.equal(await page.getByLabel('Tenant ID', { exact: true }).inputValue(), 'keep-this-draft');
    assert.equal((await fixtureCalls(page)).some(call => call.includes('/internal/v1/service-tokens')), false, 'the pending tab must not mount or fetch before confirmation');
    await page.keyboard.press('Escape');
    await page.getByRole('dialog').waitFor({ state: 'detached' });
    assert.match(page.url(), /view=tenants/);
    assert.equal(await page.getByLabel('Tenant ID', { exact: true }).inputValue(), 'keep-this-draft', 'cancelling retains the mounted form and its draft');
    await page.getByRole('tab', { name: 'Service credentials', exact: true }).click();
    await page.getByRole('dialog').getByRole('button', { name: 'Confirm and continue', exact: true }).click();
    await page.getByRole('heading', { name: 'Service credentials', exact: true }).waitFor();
    await page.locator('.credential-compact-list').waitFor();
    assert.match(page.url(), /view=service-credentials/);
    assert.equal(await tenantLink.getAttribute('aria-current'), 'page');
    assert.equal(await page.locator('.identity-workspace [role="tabpanel"]').count(), 1, 'only the current identity panel may remain in the DOM');
    assert.equal(await page.locator('.tenant-manager').count(), 0, 'discard confirmation must unmount tenant business state, not hide it');
    const serviceCallsBeforeTenants = (await fixtureCalls(page)).filter(call => call.includes('/internal/v1/service-tokens')).length;
    await page.getByRole('tab', { name: 'Tenants', exact: true }).click();
    await page.locator('.tenant-list').waitFor();
    assert.match(page.url(), /view=tenants/);
    assert.equal(await page.getByRole('dialog').count(), 0, 'a discarded draft must not leave a stale navigation guard');
    assert.equal(await page.getByLabel('Tenant ID', { exact: true }).inputValue(), '', 'reopening the discarded panel creates a clean form');
    assert.equal(await page.locator('.identity-workspace [role="tabpanel"]').count(), 1);
    assert.equal(await page.locator('.credential-compact-list').count(), 0, 'inactive service credential nodes must be removed');
    assert.equal((await fixtureCalls(page)).filter(call => call.includes('/internal/v1/service-tokens')).length, serviceCallsBeforeTenants, 'an inactive service panel must not fetch');

    await page.reload();
    await page.getByRole('heading', { name: 'Tenant management', exact: true }).waitFor();
    await page.locator('.tenant-list').waitFor();
    const lifecycleCallsBeforeBack = (await fixtureCalls(page)).filter((call) => call === 'GET /internal/v1/tenant-management').length;
    await page.goBack();
    await page.locator('.credential-compact-list').waitFor();
    assert.equal(await page.locator('.tenant-manager').count(), 0);
    assert.equal((await fixtureCalls(page)).filter((call) => call === 'GET /internal/v1/tenant-management').length, lifecycleCallsBeforeBack, 'Back to services must not fetch inactive tenants');
    await page.goBack();
    await page.locator('.tenant-list').waitFor();
    assert.equal(await page.getByLabel('Tenant ID', { exact: true }).inputValue(), '');
    assert.equal((await fixtureCalls(page)).filter((call) => call === 'GET /internal/v1/tenant-management').length, lifecycleCallsBeforeBack + 1, 'Back to tenants remounts only the selected entity');
    await page.goBack();
    await page.waitForFunction(() => new URL(location.href).searchParams.get('view') === 'overview');
    await page.locator('.identity-workspace').waitFor({ state: 'detached' });
    assert.equal(await page.locator('.identity-workspace').count(), 0);
    assert.equal((await fixtureCalls(page)).filter((call) => call === 'GET /internal/v1/tenant-management').length, lifecycleCallsBeforeBack + 1, 'leaving identity must not fetch tenant CRUD');
  } finally {
    await browser.close();
    await server.close();
  }
});

test('multi-tenant scope exposes tenant CRUD, dependency refusal, authorization boundaries, and screenshot widths', { timeout: 30_000 }, async () => {
  const executablePath = await localChromiumExecutable();
  if (!executablePath) return test.skip('a local Chromium runtime is required for tenant lifecycle assertions');
  const server = await createServer({ root: webRoot, configFile: false, logLevel: 'silent', plugins: [operatorFixturePlugin('/e2e/fixtures/tenant-management.html')], server: { host: '127.0.0.1', port: 0, strictPort: false } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ executablePath, headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
    await seedNavigationIdentity(page, 'en', true);
    await page.goto(fixture(address.port, 'multiple'));
    await page.getByRole('heading', { name: 'Tenant management', exact: true }).waitFor();
    await page.evaluate(() => localStorage.setItem('mtc-locale', 'zh-CN'));
    await page.reload();
    const chineseScope = page.getByRole('combobox', { name: '租户范围', exact: true });
    await chineseScope.waitFor();
    assert.equal(await chineseScope.locator('option[value="default"]').innerText(), '默认');
    await tenantRow(page, '默认').waitFor();
    await chineseScope.selectOption('default');
    assert.equal(await chineseScope.inputValue(), 'default', 'localized display never changes the selected external ID');
    assert.equal(await page.locator('.app-brand').innerText().then(text => text.includes('Token Center')), true, 'the product brand remains unchanged');
    await page.getByLabel('租户标识', { exact: true }).fill('尚未保存的租户');
    await chineseScope.selectOption('north');
    await page.getByRole('dialog').waitFor();
    await page.keyboard.press('Escape');
    assert.equal(await chineseScope.inputValue(), 'default');
    assert.equal(await page.getByLabel('租户标识', { exact: true }).inputValue(), '尚未保存的租户');
    await page.getByLabel('租户标识', { exact: true }).fill('');
    await page.evaluate(() => localStorage.setItem('mtc-locale', 'en'));
    await page.reload();
    await page.getByRole('heading', { name: 'Tenant management', exact: true }).waitFor();
    const scope = page.getByRole('combobox', { name: 'Tenant scope', exact: true });
    await scope.selectOption('north');
    assert.equal(await scope.inputValue(), 'north');

    await page.getByLabel('Tenant ID', { exact: true }).fill('west');
    await page.getByRole('button', { name: 'Add tenant', exact: true }).click();
    await tenantRow(page, 'west').waitFor();

    let north = tenantRow(page, 'north');
    await north.getByRole('button', { name: 'Rename', exact: true }).click();
    const renameDialog = page.getByRole('dialog', { name: 'Rename tenant', exact: true });
    await renameDialog.getByLabel('Tenant ID', { exact: true }).fill('north-renamed');
    await renameDialog.getByRole('button', { name: 'Rename', exact: true }).click();
    await tenantRow(page, 'north-renamed').waitFor();
    north = tenantRow(page, 'north-renamed');

    await north.getByRole('button', { name: 'Archive', exact: true }).click();
    const archiveDialog = page.getByRole('dialog', { name: 'Archive tenant', exact: true });
    await archiveDialog.waitFor();
    await page.keyboard.press('Escape');
    assert.equal(await page.getByRole('dialog', { name: 'Archive tenant', exact: true }).count(), 0, 'Escape must dismiss the tenant action dialog');
    await north.getByRole('button', { name: 'Archive', exact: true }).click();
    await page.getByRole('dialog', { name: 'Archive tenant', exact: true }).getByRole('button', { name: 'Archive', exact: true }).click();
    await page.getByRole('button', { name: 'Show Archived (1)', exact: true }).waitFor();
    assert.equal(await tenantRow(page, 'north-renamed').count(), 0, 'archived tenants must be hidden by default');
    await page.getByRole('button', { name: 'Show Archived (1)', exact: true }).click();
    north = tenantRow(page, 'north-renamed');
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
    }
    await captureNavigationIdentity(page, 'identity', 'tenant-lifecycle', 'en');

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

test('identity tabs retain legacy deep links, separate denial boundaries and bilingual screenshots for normal, empty and loading states', { timeout: 30_000 }, async () => {
  const executablePath = await localChromiumExecutable();
  if (!executablePath) return test.skip('a Chromium runtime is required for identity navigation assertions');
  const server = await createServer({ root: webRoot, configFile: false, logLevel: 'silent', plugins: [operatorFixturePlugin('/e2e/fixtures/tenant-management.html')], server: { host: '127.0.0.1', port: 0, strictPort: false } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ executablePath, headless: true });
  try {
    for (const locale of ['zh-CN', 'en']) {
      const title = locale === 'en' ? 'Identity management' : '身份管理';
      for (const scenario of ['default', 'slow', 'management-denied', 'management-error', 'background-refresh', 'services-denied', 'services-empty'] as const) {
        const page = await browser.newPage();
        await seedNavigationIdentity(page, locale);
        const view = scenario.startsWith('services-') ? 'service-credentials' : 'tenants';
        await page.goto(fixture(address.port, scenario, view));
        await page.getByRole('heading', { name: title, exact: true }).waitFor();
        const selectedTab = page.getByRole('tab', { name: view === 'tenants' ? locale === 'en' ? 'Tenants' : '租户' : locale === 'en' ? 'Service credentials' : '服务凭据', exact: true });
        assert.equal(await selectedTab.getAttribute('aria-selected'), 'true');
        if (scenario === 'slow') await page.waitForFunction(() => Boolean(window.tenantFixture.release));
        else if (scenario === 'management-denied') await page.getByRole('alert').getByText('Tenant management denied', { exact: true }).waitFor();
        else if (scenario === 'management-error') await page.getByRole('alert').getByText('Tenant management temporarily unavailable', { exact: true }).waitFor();
        else if (scenario === 'services-denied') await page.getByRole('alert').getByText('Service credential access denied', { exact: true }).waitFor();
        else if (scenario === 'services-empty') await page.locator('.credential-compact-list').waitFor();
        else await page.locator('.tenant-list').waitFor();
        if (scenario === 'background-refresh') {
          await page.evaluate(() => { window.tenantFixture.holdNext = true; });
          await page.getByLabel(locale === 'en' ? 'Tenant ID' : '租户标识', { exact: true }).fill('west');
          await page.getByRole('button', { name: locale === 'en' ? 'Add tenant' : '新增租户', exact: true }).click();
          await page.waitForFunction(() => Boolean(window.tenantFixture.release));
          assert.equal(await tenantRow(page, locale === 'en' ? 'default' : '默认').isVisible(), true, 'background lifecycle refresh retains the previous real tenant list');
          assert.equal(await tenantRow(page, 'west').count(), 0, 'held refresh cannot publish the new list early');
        }
        assert.equal(await page.locator('.identity-workspace [role="tabpanel"]').count(), 1, 'loading and denial must not preserve an inactive identity panel');
        if (view === 'service-credentials') assert.equal(await page.locator('.tenant-manager').count(), 0);
        else assert.equal(await page.locator('.credential-compact-list').count(), 0);
        const calls = await fixtureCalls(page);
        if (view === 'tenants') assert.equal(calls.some(call => call.includes('/internal/v1/service-tokens')), false, 'tenant access must not fetch service credentials before that tab is opened');
        else assert.equal(calls.some(call => call.includes('/internal/v1/tenant-management')), false, 'service credential access must not load tenant management');
        const artifactScenario = scenario === 'default' ? 'ready' : scenario === 'slow' ? 'initial-loading'
          : scenario.endsWith('-denied') ? 'permission-failure' : scenario === 'management-error' ? 'error'
            : scenario === 'services-empty' ? 'empty' : scenario;
        await captureNavigationIdentity(page, scenario === 'services-denied' ? 'identity-service-credentials' : 'identity', artifactScenario, locale);
        if (scenario === 'slow' || scenario === 'background-refresh') {
          await page.evaluate(() => window.tenantFixture.release?.());
          await page.locator('.tenant-list').waitFor();
          if (scenario === 'background-refresh') await tenantRow(page, 'west').waitFor();
        }
        await page.reload();
        await page.getByRole('heading', { name: title, exact: true }).waitFor();
        assert.equal(await selectedTab.getAttribute('aria-selected'), 'true', 'refresh restores the selected identity entity');
        await page.close();
      }
    }
  } finally { await browser.close(); await server.close(); }
});
