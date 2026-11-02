import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer as createServer } from './support/isolated-vite-server.js';

declare global { interface Window { routeScopeDeletes: Array<{ path: string; query: string }> } }

test('route deletion permanently removes configuration and explains retained history', { timeout: 45_000 }, async () => {
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer!.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'zh-CN'));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/route-form-scope.html?existing=1&disabled=1`);
    await page.locator('[data-resource-list-status-filter]').getByRole('button').click();
    const route = page.locator('[data-route-id=\"existing-route\"]');
    await route.getByRole('button', { name: '删除', exact: true }).click();
    const confirmation = page.getByRole('dialog');
    await confirmation.getByText(/永久删除路由 saved-route 的配置/).waitFor();
    await confirmation.getByText(/历史请求和费用记录保留/).waitFor();
    await confirmation.getByRole('button', { name: '确认继续', exact: true }).click();
    await page.getByText('路由配置已删除，历史请求和费用记录保留', { exact: true }).waitFor();
    assert.deepEqual(await page.evaluate(() => window.routeScopeDeletes), [{
      path: '/internal/v1/model-routes/existing-route',
      query: '?tenant_external_id=fixture&expected_updated_at=42',
    }]);
    assert.equal(await page.locator('[data-route-id=\"existing-route\"]').count(), 0);
  } finally {
    await browser.close();
    await server.close();
  }
});
