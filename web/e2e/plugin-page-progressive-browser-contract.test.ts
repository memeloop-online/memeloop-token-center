import assert from 'node:assert/strict';
import { existsSync, mkdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer as createServer } from './support/isolated-vite-server.js';

test('plugin catalog is usable without unrelated route code or unopened configuration forms', { timeout: 45_000 }, async (context) => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    context.skip('Chromium not installed'); return;
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer!.address();
  assert.ok(address && typeof address === 'object');
  const browser = await chromium.launch({ headless: true });
  let releaseUnrelatedRoutes: () => void = () => undefined;
  let releaseConfigurationForm: () => void = () => undefined;
  const unrelatedRouteGate = new Promise<void>((resolve) => { releaseUnrelatedRoutes = resolve; });
  const configurationFormGate = new Promise<void>((resolve) => { releaseConfigurationForm = resolve; });
  try {
    const page = await browser.newPage({ viewport: { width: 390, height: 844 } });
    await page.addInitScript(() => {
      localStorage.setItem('mtc-locale', 'en');
      localStorage.setItem('mtc.operator.service-credential.v1', 'operator-test');
    });
    const unrelatedRequests: string[] = [];
    for (const modulePath of [
      '/src/operator/pages/ManagementPages.tsx',
      '/src/operator/pages/OperatorPages.tsx',
      '/src/operator/pages/RequestsPage.tsx',
      '/src/operator/pages/SessionsPage.tsx',
      '/src/operator/pages/SystemSettingsPage.tsx',
      '/src/operator/TenantManager.tsx',
    ]) {
      await page.route(`**${modulePath}*`, async (route) => {
        unrelatedRequests.push(modulePath);
        await unrelatedRouteGate;
        await route.continue();
      });
    }
    let configurationModuleRequests = 0;
    await page.route('**/src/operator/PluginConfigurationForm.tsx*', async (route) => {
      configurationModuleRequests += 1;
      await configurationFormGate;
      await route.continue();
    });
    let catalogReads = 0;
    let configurationReads = 0;
    await page.route('**/internal/v1/**', async (route) => {
      const url = new URL(route.request().url());
      assert.equal(route.request().headers().authorization, 'Bearer operator-test');
      assert.equal(route.request().method(), 'GET', 'this read-only contract must not mutate plugin state');
      if (url.pathname === '/internal/v1/tenants') return route.fulfill({ json: [{ external_id: 'alpha' }] });
      if (url.pathname === '/internal/v1/plugins/runtime-access') return route.fulfill({ json: { can_view_runtime: true, can_manage_runtime: true } });
      if (url.pathname === '/internal/v1/plugin-runtime') {
        return route.fulfill({ json: {
          current: { revision: 2, inventory_id: 'current-1', reason: 'publish', created_at: 1 },
          candidates: [{ inventory_id: 'candidate-2', staged: true, plugins: { first: ['1.0.0'] } }],
        } });
      }
      if (url.pathname === '/internal/v1/plugin-runtime/history') {
        return route.fulfill({ json: {
          runtime_enabled: true, installation_enabled: true, installations: [],
          revisions: [{ revision: 2, inventory_id: 'current-1', reason: 'publish', created_at: 1 }],
          audit: [{ id: 'audit-1', actor: 'operator-test', action: 'publish', inventory_id: 'current-1', revision: 2, outcome: 'success', created_at: 1 }],
        } });
      }
      if (url.pathname === '/internal/v1/plugins') {
        catalogReads += 1;
        return route.fulfill({ json: [{
          id: 'first', version: '1.0.0', wit_version: '0.2.0', capabilities: [],
          contributions: { configuration: { default: {}, schema: { type: 'object', properties: { mode: { type: 'string', title: 'Mode' } } } } },
        }] });
      }
      if (url.pathname === '/internal/v1/plugins/first/configuration') {
        configurationReads += 1;
        return route.fulfill({ json: { source: 'tenant', scope_version: 1, value: { mode: 'ready' } } });
      }
      return route.fulfill({ status: 404, json: { error: { message: 'Unexpected fixture request' } } });
    });
    await page.goto(`http://127.0.0.1:${address.port}/operator?view=plugins`);
    const plugin = page.locator('.account-list > .managed-resource').filter({ has: page.getByText('first', { exact: true }) });
    await plugin.waitFor();
    await page.getByText('candidate-2', { exact: true }).waitFor();
    assert.deepEqual(unrelatedRequests, [], 'the selected plugin route must not wait for unrelated management page modules');
    assert.equal(catalogReads, 1, 'shell registration and plugin management share one catalog read');
    assert.equal(configurationModuleRequests, 0, 'an unopened configuration disclosure does not load schema form code');
    assert.equal(configurationReads, 0, 'catalog rendering does not fan out configuration reads');
    const headingTypography = await page.locator('.plugin-management-flow').evaluate((flow) => {
      const baseFamily = getComputedStyle(flow).fontFamily;
      return [...flow.querySelectorAll<HTMLElement>(':scope > .panel h2')].map((heading) => ({
        family: getComputedStyle(heading).fontFamily,
        weight: Number(getComputedStyle(heading).fontWeight),
        baseFamily,
      }));
    });
    assert.equal(headingTypography.length, 2, 'runtime and catalog management headings are both present');
    assert.ok(headingTypography.every(({ family, baseFamily, weight }) => family === baseFamily && weight >= 600),
      'plugin management headings use the same base sans typography as other Operator management workspaces');

    const activation = page.getByRole('checkbox', { name: 'Confirm global activation' });
    await activation.focus();
    await page.keyboard.press('Space');
    assert.equal(await activation.isChecked(), true, 'the labelled activation control remains keyboard operable');
    const compactRecords = page.getByRole('button', { name: 'Tasks and version records' });
    await compactRecords.focus();
    await page.keyboard.press('Enter');
    await page.getByRole('heading', { name: 'Installation tasks' }).waitFor();
    await compactRecords.press('Enter');

    const artifacts = fileURLToPath(new URL('../e2e-artifacts/ui-system/plugin-management/', import.meta.url));
    mkdirSync(artifacts, { recursive: true });
    for (const width of [390, 1440]) {
      await page.setViewportSize({ width, height: width === 390 ? 844 : 900 });
      await page.evaluate(() => { (document.activeElement as HTMLElement | null)?.blur(); window.scrollTo(0, 0); });
      const geometry = await page.evaluate(() => ({ viewport: innerWidth, scrollWidth: document.documentElement.scrollWidth }));
      assert.ok(geometry.scrollWidth <= geometry.viewport, `${width}px plugin management must not overflow horizontally`);
      await page.screenshot({ path: `${artifacts}/ready-${width}.png`, fullPage: true });
    }

    const configurationModuleRequested = page.waitForRequest((request) => new URL(request.url()).pathname.endsWith('/src/operator/PluginConfigurationForm.tsx'));
    const configurationReadCompleted = page.waitForResponse((response) => new URL(response.url()).pathname === '/internal/v1/plugins/first/configuration');
    const configurationButton = plugin.getByRole('button', { name: 'View and edit configuration' });
    await configurationButton.focus();
    await page.keyboard.press('Enter');
    await configurationModuleRequested;
    assert.equal((await configurationReadCompleted).status(), 200);
    assert.equal(configurationReads, 1, 'one disclosure action owns one configuration read while schema code loads in parallel');
    releaseConfigurationForm();
    await plugin.getByLabel('Mode').waitFor();
    assert.equal(await plugin.getByLabel('Mode').inputValue(), 'ready');
    assert.equal(configurationReads, 1);
    await plugin.getByLabel('Mode').fill('unsaved draft');
    await configurationButton.press('Space');
    assert.equal(await plugin.getByLabel('Mode').isHidden(), true);
    await configurationButton.press('Space');
    await plugin.getByLabel('Mode').waitFor();
    assert.equal(await plugin.getByLabel('Mode').inputValue(), 'unsaved draft', 'collapsing configuration must not discard an unsaved draft');
    assert.equal(configurationModuleRequests, 1, 'reopening the configuration surface reuses loaded form code');
    assert.equal(configurationReads, 1, 'reopening a loaded configuration does not duplicate the read');
  } finally {
    releaseConfigurationForm();
    releaseUnrelatedRoutes();
    await browser.close();
    await server.close();
  }
});
