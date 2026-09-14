import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
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
    const page = await browser.newPage({ viewport: { width: 1024, height: 800 } });
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
      if (url.pathname === '/internal/v1/tenants') return route.fulfill({ json: [{ external_id: 'alpha' }] });
      if (url.pathname === '/internal/v1/plugins/runtime-access') return route.fulfill({ json: { can_view_runtime: false, can_manage_runtime: false } });
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
    const plugin = page.locator('.managed-resource').filter({ has: page.getByText('first', { exact: true }) });
    await plugin.waitFor();
    assert.deepEqual(unrelatedRequests, [], 'the selected plugin route must not wait for unrelated management page modules');
    assert.equal(catalogReads, 1, 'shell registration and plugin management share one catalog read');
    assert.equal(configurationModuleRequests, 0, 'an unopened configuration disclosure does not load schema form code');
    assert.equal(configurationReads, 0, 'catalog rendering does not fan out configuration reads');

    const configurationModuleRequested = page.waitForRequest((request) => new URL(request.url()).pathname.endsWith('/src/operator/PluginConfigurationForm.tsx'));
    await plugin.locator('summary').click();
    await configurationModuleRequested;
    assert.equal(configurationReads, 0, 'configuration data waits for its form module instead of racing every catalog row');
    releaseConfigurationForm();
    await plugin.getByLabel('Mode').waitFor();
    assert.equal(await plugin.getByLabel('Mode').inputValue(), 'ready');
    assert.equal(configurationReads, 1);
  } finally {
    releaseConfigurationForm();
    releaseUnrelatedRoutes();
    await browser.close();
    await server.close();
  }
});
