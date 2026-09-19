import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

test('an installed runtime module renders without rebuilding MTC and failures stay local', { timeout: 60_000 }, async () => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required');
    return test.skip('Chromium is required');
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 1024, height: 768 } });
    const dataCalls: Array<{ tenant: string | null; authorization: string | undefined }> = [];
    let moduleLoads = 0;
    await page.route('**/ui-assets/plugins/**', async (route) => {
      moduleLoads += 1;
      return route.fulfill({
        contentType: 'text/javascript; charset=utf-8',
        body: `export function activateOperatorUi({ React, defineOperatorUiPackage }) {
          const { useEffect, useState } = React;
          function Workspace({ api, contribution, tenantExternalId }) {
            const [status, setStatus] = useState('loading');
            useEffect(() => { const controller = new AbortController(); api.loadServiceData('health', controller.signal).then((value) => setStatus(String(value.data.status))); return () => controller.abort(); }, [api]);
            return React.createElement('div', { 'data-testid': 'trusted-plugin-workspace' },
              React.createElement('strong', null, contribution.label), React.createElement('span', null, tenantExternalId), React.createElement('span', null, status),
              React.createElement('button', { type: 'button', onClick: () => api.navigate('providers') }, 'Open providers'));
          }
          function Broken() { throw new Error('fixture failure'); }
          function After() { return React.createElement('div', { 'data-testid': 'healthy-page-extension' }, 'Extension remains available'); }
          return defineOperatorUiPackage({ apiVersion: 'operator-ui-package-v1', pluginId: 'component-dashboard', compatiblePluginVersions: ['1.0.0'], components: { workspace: Workspace, broken: Broken, after: After } });
        }`,
      });
    });
    await page.route('**/internal/v1/**', async (route) => {
      const url = new URL(route.request().url());
      if (url.pathname === '/internal/v1/tenants') return route.fulfill({ json: [{ external_id: 'alpha' }] });
      if (url.pathname === '/internal/v1/plugins') return route.fulfill({ json: [{
        id: 'component-dashboard', version: '1.0.0', wit_version: '0.2.0', capabilities: [],
        contributions: {
          operator_ui: [
            { id: 'workspace', slot: 'operator.sidebar.tab', category: { id: 'monitoring' }, route: 'workspace', label: 'Custom workspace', icon: 'plug', renderer: 'component_v1', module_entry: 'assets/operator-ui.mjs', module_sha256: 'sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', component_id: 'workspace' },
            { id: 'broken', slot: 'operator.page.before', target_route: 'providers', label: 'Broken extension', icon: 'plug', renderer: 'component_v1', module_entry: 'assets/operator-ui.mjs', module_sha256: 'sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', component_id: 'broken' },
            { id: 'after', slot: 'operator.page.after', target_route: 'providers', label: 'Healthy extension', icon: 'plug', renderer: 'component_v1', module_entry: 'assets/operator-ui.mjs', module_sha256: 'sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', component_id: 'after' },
          ],
          service_data: [{ id: 'health' }],
        },
      }] });
      if (url.pathname === '/internal/v1/plugins/component-dashboard/data/health') {
        dataCalls.push({ tenant: url.searchParams.get('tenant_external_id'), authorization: route.request().headers().authorization });
        return route.fulfill({ json: { data: { status: 'ready' }, partial: false, provenance: { plugin_id: 'component-dashboard', endpoint_id: 'health', origin: 'https://example.com', fetched_at: Date.now(), source: 'component', freshness: 'fresh', last_attempt_at: Date.now(), next_attempt_at: Date.now() + 30_000, consecutive_failures: 0 } } });
      }
      if (url.pathname === '/internal/v1/upstreams') return route.fulfill({ json: [] });
      return route.fulfill({ json: [] });
    });
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/operator-plugin-component.html`);
    await page.getByTestId('trusted-plugin-workspace').getByText('ready', { exact: true }).waitFor();
    assert.deepEqual(dataCalls, [{ tenant: 'alpha', authorization: 'Bearer mts_component_fixture' }]);
    await page.getByRole('button', { name: 'Open providers' }).click();
    await page.locator('.provider-directory').waitFor();
    await page.getByTestId('healthy-page-extension').waitFor();
    await page.getByRole('alert').getByText('This plugin view could not be loaded.').waitFor();
    assert.equal(moduleLoads, 1);
  } finally { await browser.close(); await server.close(); }
});
