import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer as createServer } from './support/isolated-vite-server.js';

// Exercise the real Operator and lazy pages; only the HTTP boundary is mocked.
// Real empty-database acceptance remains a separate deployment check.
test('confirmed empty tenant scope allows global plugin management and first tenant creation', { timeout: 60_000 }, async (context) => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    context.skip('Chromium not installed'); return;
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer!.address();
  assert.ok(address && typeof address === 'object');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    await page.addInitScript(() => {
      localStorage.setItem('mtc-locale', 'en');
      localStorage.setItem('mtc.operator.service-credential.v1', 'bootstrap-test');
    });
    const calls: string[] = [];
    const tenants: { external_id: string; status: string }[] = [];
    await page.route('**/internal/v1/**', async (route) => {
      const url = new URL(route.request().url());
      const method = route.request().method();
      calls.push(`${method} ${url.pathname}${url.search}`);
      if (url.pathname === '/internal/v1/tenants') return route.fulfill({ json: tenants });
      if (url.pathname === '/internal/v1/tenant-management') {
        if (method === 'POST') {
          const tenant = { external_id: route.request().postDataJSON().external_id, status: 'active' };
          tenants.push(tenant);
          return route.fulfill({ json: tenant });
        }
        return route.fulfill({ json: tenants });
      }
      if (url.pathname === '/internal/v1/plugins/runtime-access') return route.fulfill({ json: { can_view_runtime: true, can_manage_runtime: true } });
      if (url.pathname === '/internal/v1/plugin-runtime') return route.fulfill({ json: { current: null, candidates: [] } });
      if (url.pathname === '/internal/v1/plugin-runtime/history') return route.fulfill({ json: { runtime_enabled: true, installation_enabled: true, installations: [], revisions: [], audit: [] } });
      if (url.pathname === '/internal/v1/plugin-runtime/installer-network') return route.fulfill({ json: method === 'PUT' ? route.request().postDataJSON() : { mode: 'default' } });
      if (url.pathname === '/internal/v1/plugins') return route.fulfill({ json: [{ id: 'first', version: '1.0.0', wit_version: '0.2.0', capabilities: [], contributions: { configuration: { default: {}, schema: { type: 'object' } }, service_data: [{ id: 'summary' }], operator_ui: ['plugins', 'tenants'].map(target_route => ({ id: `${target_route}-summary`, slot: 'operator.page.before', target_route, label: 'Tenant extension', icon: 'chart', renderer: 'typed_data_v1', data_endpoint: 'summary' })) } }] });
      if (url.pathname === '/internal/v1/plugins/first/configuration') return route.fulfill({ json: { source: 'tenant', scope_version: 1, value: {} } });
      return route.fulfill({ status: 500, json: { error: 'Unexpected tenant resource request' } });
    });
    await page.goto(`http://127.0.0.1:${address.port}/operator?view=plugins`);
    await page.getByRole('button', { name: 'Install plugin', exact: true }).click();
    assert.equal(await page.getByRole('heading', { name: 'Plugin settings', exact: true }).count(), 0);
    await page.getByRole('button', { name: 'Download network', exact: true }).click();
    await page.getByLabel('Download network', { exact: true }).selectOption('group');
    await page.getByText('Select a tenant to use its proxy groups.', { exact: true }).waitFor();
    assert.equal(await page.getByLabel('Proxy group', { exact: true }).isDisabled(), true);
    assert.equal(calls.some(call => call.includes('/groups') || call.includes('/configuration') || call.includes('/data/')), false);
    await page.getByLabel('Download network', { exact: true }).selectOption('custom');
    await page.getByLabel('Proxy URL', { exact: true }).fill('http://proxy.example:8080');
    await page.getByRole('button', { name: 'Save network', exact: true }).click();
    await page.getByText('Download network saved.', { exact: true }).waitFor();
    assert.equal(await page.getByRole('button', { name: 'Read release', exact: true }).isEnabled(), true);
    await page.getByRole('button', { name: 'Add tenant', exact: true }).click();
    await page.getByLabel('Tenant ID', { exact: true }).waitFor();
    assert.equal(calls.some(call => call.includes('/data/')), false, 'global tenant creation must not mount tenant plugin extensions');
    await page.getByLabel('Tenant ID', { exact: true }).fill('first-tenant');
    await page.getByRole('button', { name: 'Add tenant', exact: true }).click();
    await page.waitForFunction(() => localStorage.getItem('mtc.operator.tenant.v1') === 'first-tenant');
    // Reload through a real application route after discovery selects the new tenant.
    await page.goto(`http://127.0.0.1:${address.port}/operator?view=plugins`);
    const configurationRead = page.waitForResponse(response => new URL(response.url()).pathname === '/internal/v1/plugins/first/configuration');
    await page.getByRole('button', { name: 'View and edit configuration', exact: true }).click();
    await configurationRead;
    assert.ok(calls.includes('GET /internal/v1/plugins/first/configuration?tenant_external_id=first-tenant'));
    assert.equal(calls.includes('GET /internal/v1/plugins/first/configuration'), false);
  } finally {
    await browser.close();
    await server.close();
  }
});

for (const view of ['plugins', 'tenants', 'providers', 'service-credentials', 'system-settings']) {
  test(`pending or failed initial tenant discovery never mounts ${view} resources`, { timeout: 45_000 }, async (context) => {
    if (!existsSync(chromium.executablePath())) {
      if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
      context.skip('Chromium not installed'); return;
    }
    const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
    await server.listen();
    const address = server.httpServer!.address();
    assert.ok(address && typeof address === 'object');
    const browser = await chromium.launch({ headless: true });
    let release = () => {};
    const gate = new Promise<void>(resolve => { release = resolve; });
    try {
      const page = await browser.newPage();
      await page.addInitScript(() => {
        localStorage.setItem('mtc-locale', 'en');
        localStorage.setItem('mtc.operator.service-credential.v1', 'bootstrap-test');
      });
      const calls: string[] = [];
      let unavailable = true;
      await page.route('**/internal/v1/**', async route => {
        const path = new URL(route.request().url()).pathname;
        calls.push(path);
        if (path === '/internal/v1/tenants') {
          await gate;
          return unavailable
            ? route.fulfill({ status: 503, json: { error: 'Discovery unavailable' } })
            : route.fulfill({ json: [] });
        }
        return route.fulfill({ json: [] });
      });
      const discoveryStarted = page.waitForRequest(request => new URL(request.url()).pathname === '/internal/v1/tenants');
      await page.goto(`http://127.0.0.1:${address.port}/operator?view=${view}`);
      await discoveryStarted;
      await page.locator('[id^="operator-panel-"]').waitFor();
      assert.deepEqual(calls, ['/internal/v1/tenants']);
      assert.equal(await page.getByText('No tenants are available to this credential.', { exact: true }).count(), 0, 'loading is not an empty scope');
      release();
      await page.getByRole('alert').filter({ hasText: 'temporarily unavailable' }).waitFor();
      assert.deepEqual(calls, ['/internal/v1/tenants']);
      assert.equal(await page.getByText('No tenants are available to this credential.', { exact: true }).count(), 0, 'failed discovery is not an empty scope');
      unavailable = false;
      await page.getByRole('alert').getByRole('button', { name: 'Retry', exact: true }).click();
      await page.locator('.console-context').getByText('No tenants are available to this credential.', { exact: true }).waitFor();
      if (view === 'tenants') await page.getByLabel('Tenant ID', { exact: true }).waitFor();
      if (view === 'plugins') await page.getByText('Create a tenant to configure plugins.', { exact: true }).waitFor();
      const allowed = new Set(['/internal/v1/tenants', '/internal/v1/plugins', ...(view === 'tenants' ? ['/internal/v1/tenant-management'] : view === 'plugins' ? ['/internal/v1/plugins/runtime-access'] : [])]);
      assert.deepEqual(calls.filter(path => !allowed.has(path)), [], 'confirmed empty discovery must not enable tenant-scoped resources');
    } finally {
      release();
      await browser.close();
      await server.close();
    }
  });
}
