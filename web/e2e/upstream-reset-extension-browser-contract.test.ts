import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer as createServer } from './support/isolated-vite-server.js';

test('reset extensions receive only the current account and render before quota discovery', { timeout: 60_000 }, async () => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    return test.skip('Chromium required');
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    const unexpected: string[] = [];
    const errors: string[] = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.route('**/internal/**', route => { unexpected.push(route.request().url()); return route.abort(); });
    await page.route('**/ui-assets/plugins/**', route => route.fulfill({ contentType: 'text/javascript', body: `
      export function activateOperatorUi({React,defineOperatorUiPackage}) {
        function Settings({context,tenantExternalId}) {
          const account=context.upstreamAccount;
          if (tenantExternalId !== account.tenantExternalId) throw new Error('Tenant mismatch');
          return React.createElement('label',{'data-testid':'account-settings','data-account':account.id,'data-tenant':account.tenantExternalId,'data-provider':account.provider},
            React.createElement('input',{type:'checkbox'}), 'Automatically use expiring credits for '+account.name);
        }
        return defineOperatorUiPackage({apiVersion:'operator-ui-package-v1',pluginId:'reset-extension',compatiblePluginVersions:['1.0.0'],components:{settings:Settings}});
      }` }));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/upstream-reset-extension.html`);
    const settings = page.locator('.upstream-quota-reset').getByTestId('account-settings');
    await settings.waitFor();
    assert.equal(await settings.getAttribute('data-account'), 'account-one');
    assert.equal(await settings.getAttribute('data-provider'), 'openai-codex');
    assert.equal(await page.getByTestId('navigation-count').innerText(), '0');
    assert.equal(await page.getByRole('button', { name: 'Reset upstream quota', exact: true }).isDisabled(), true);
    await page.getByRole('button', { name: 'Change account' }).click();
    await page.waitForFunction(() => document.querySelector('[data-testid="account-settings"]')?.getAttribute('data-account') === 'account-two');
    await page.getByRole('button', { name: 'Change tenant' }).click();
    await page.waitForFunction(() => document.querySelector('[data-testid="account-settings"]')?.getAttribute('data-tenant') === 'tenant-two');
    for (const width of [1440, 390]) {
      await page.setViewportSize({ width, height: 900 });
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
    }
    await page.getByRole('button', { name: 'Remove extension' }).click();
    await settings.waitFor({ state: 'detached' });
    assert.equal(await page.getByRole('button', { name: 'Reset upstream quota', exact: true }).count(), 1);
    assert.deepEqual(unexpected, []);
    assert.deepEqual(errors, []);
  } finally { await browser.close(); await server.close(); }
});
