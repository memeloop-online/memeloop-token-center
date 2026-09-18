import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

test('trusted component package renders a plugin tab and uses scoped host data', { timeout: 60_000 }, async () => {
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
    await page.route('**/internal/v1/**', async (route) => {
      const url = new URL(route.request().url());
      if (url.pathname === '/internal/v1/tenants') return route.fulfill({ json: [{ external_id: 'alpha' }] });
      if (url.pathname === '/internal/v1/plugins') return route.fulfill({ json: [{
        id: 'component-dashboard', version: '1.0.0', wit_version: '0.2.0', capabilities: [],
        contributions: {
          operator_ui: [{ id: 'workspace', slot: 'operator.sidebar.tab', category: { id: 'monitoring' }, route: 'workspace', label: 'Custom workspace', icon: 'plug', renderer: 'component_v1', component_id: 'workspace' }],
          service_data: [{ id: 'health' }],
        },
      }] });
      if (url.pathname === '/internal/v1/plugins/component-dashboard/data/health') {
        dataCalls.push({ tenant: url.searchParams.get('tenant_external_id'), authorization: route.request().headers().authorization });
        return route.fulfill({ json: { data: { status: 'ready' }, partial: false, provenance: { plugin_id: 'component-dashboard', endpoint_id: 'health', origin: 'https://example.com', fetched_at: Date.now(), source: 'network' } } });
      }
      if (url.pathname === '/internal/v1/upstreams') return route.fulfill({ json: [] });
      return route.fulfill({ json: [] });
    });
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/operator-plugin-component.html`);
    await page.getByTestId('trusted-plugin-workspace').getByText('ready', { exact: true }).waitFor();
    assert.deepEqual(dataCalls, [{ tenant: 'alpha', authorization: 'Bearer mts_component_fixture' }]);
    await page.getByRole('button', { name: 'Open providers' }).click();
    await page.locator('.provider-directory').waitFor();
  } finally { await browser.close(); await server.close(); }
});
