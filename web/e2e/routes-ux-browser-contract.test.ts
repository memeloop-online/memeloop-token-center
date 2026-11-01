import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer as createServer } from './support/isolated-vite-server.js';

test('route groups precede the catalog and model search includes later authorized pages', { timeout: 60_000 }, async () => {
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer!.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  const evidence = fileURLToPath(new URL('../e2e-artifacts/ui-system/routes/', import.meta.url));
  await mkdir(evidence, { recursive: true });
  try {
    for (const width of [1440, 390]) {
      const page = await browser.newPage({ viewport: { width, height: 1000 } });
      const errors: string[] = [];
      page.on('pageerror', error => errors.push(error.message));
      await page.addInitScript(() => localStorage.setItem('mtc-locale', 'zh-CN'));
      await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/route-form-scope.html?paged=1&groups=1`);
      await page.locator('[data-route-id="route-100"]').waitFor();
      assert.equal(await page.locator('.model-route-list tbody tr').count(), 101);
      const beforeList = await page.evaluate(() => {
        const managers = document.querySelector('.routing-group-managers')!;
        const list = document.querySelector('.model-route-list')!;
        return Boolean(managers.compareDocumentPosition(list) & Node.DOCUMENT_POSITION_FOLLOWING);
      });
      assert.equal(beforeList, true);
      const members = page.locator('[data-group-kind="route"]').getByRole('combobox', { name: '路由成员', exact: true });
      await members.fill('kimi-k3-256k');
      const choice = page.getByRole('option').filter({ hasText: 'Kimi K3 (256K)' });
      await choice.waitFor();
      assert.equal((await choice.innerText()).includes('route-100'), false);
      await members.press('Escape');
      await page.getByRole('searchbox', { name: '搜索模型名称' }).fill('kimi-k3-256k');
      assert.equal(await page.locator('.model-route-list tbody tr').count(), 1);
      assert.match(await page.locator('.model-route-list tbody').innerText(), /Kimi K3 \(256K\)/);
      await page.screenshot({ path: `${evidence}catalog-${width}.png`, fullPage: true });
      assert.deepEqual(errors, []);
      await page.close();
    }
  } finally {
    await browser.close();
    await server.close();
  }
});
