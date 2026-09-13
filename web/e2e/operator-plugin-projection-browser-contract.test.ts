import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

test('real Operator route authenticates projection reads and discards prior tenant data', { timeout: 60_000 }, async () => {
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
    const page = await browser.newPage({ viewport: { width: 390, height: 844 } });
    const calls: Array<{ tenant: string | null; authorization: string | undefined }> = [];
    const errors: string[] = [];
    page.on('pageerror', (error) => errors.push(error.message));
    await page.route('**/internal/v1/**', async (route) => {
      const url = new URL(route.request().url());
      if (url.pathname === '/internal/v1/tenants') return route.fulfill({ json: [{ external_id: 'alpha' }, { external_id: 'beta' }] });
      if (url.pathname === '/internal/v1/plugins') return route.fulfill({ json: [{
        id: 'dashboard', version: '1.0.0', wit_version: '0.2.0',
        capabilities: [{ kind: 'http', allowed_origins: ['https://example.com'] }],
        contributions: {
          operator_ui: [{ id: 'summary', slot: 'operator.sidebar.tab', category: { id: 'monitoring' }, route: 'summary-page', label: 'Projection summary', icon: 'chart', renderer: 'typed_data_v1', presentation: 'projection_v1', data_endpoint: 'summary-data' }],
          service_data: [{ id: 'summary-data' }],
        },
      }] });
      if (url.pathname === '/internal/v1/plugins/dashboard/data/summary-data') {
        const tenant = url.searchParams.get('tenant_external_id');
        calls.push({ tenant, authorization: route.request().headers().authorization });
        if (tenant === 'beta') return route.fulfill({ status: 403, json: { error: { message: 'Tenant feed denied' } } });
        return route.fulfill({ json: {
          data: { schema_version: 1, plugin_id: 'dashboard', slot_id: 'summary', components: [{ kind: 'metric', label: 'Tenant requests', value: 'alpha-private-42' }, { kind: 'link', label: 'Details', href: 'https://example.com/details' }] },
          partial: false, provenance: { plugin_id: 'dashboard', endpoint_id: 'summary-data', origin: 'https://example.com', fetched_at: Date.now(), source: 'network' },
        } });
      }
      return route.fulfill({ json: [] });
    });
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/operator-plugin-projection.html`);
    await page.getByText('alpha-private-42', { exact: true }).waitFor();
    assert.equal(await page.locator('.plugin-contribution-page .plugin-ui-slot').count(), 1);
    assert.equal(await page.getByRole('link', { name: 'Details', exact: true }).getAttribute('href'), 'https://example.com/details');
    await page.locator('.tenant-scope-switcher select').selectOption('beta');
    await page.getByText('Plugin data is currently unavailable.', { exact: true }).waitFor();
    assert.equal(await page.getByText('alpha-private-42', { exact: true }).count(), 0);
    assert.deepEqual(calls, [
      { tenant: 'alpha', authorization: 'Bearer mts_projection_fixture' },
      { tenant: 'beta', authorization: 'Bearer mts_projection_fixture' },
    ]);
    assert.deepEqual(errors, []);
  } finally { await browser.close(); await server.close(); }
});
