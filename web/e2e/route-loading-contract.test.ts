import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { readdir } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

import { chromium } from 'playwright';
import { createServer } from 'vite';

const webRoot = fileURLToPath(new URL('..', import.meta.url));

declare global {
  interface Window { routeLoadingFixture: { calls: string[] } }
}

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

test('route credentials stay off the list critical path and load inside an opened form', { timeout: 30_000 }, async () => {
  const executablePath = await localChromiumExecutable();
  if (!executablePath) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required for the route loading CI gate');
    return test.skip('a local Chromium runtime is required for route loading behavior assertions');
  }
  const server = await createServer({ root: webRoot, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0, strictPort: false } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ executablePath, headless: true });
  try {
    const page = await browser.newPage();
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/route-loading.html`);
    await page.getByText('fixture-public-model', { exact: true }).waitFor();
    const initialCalls = await page.evaluate(() => window.routeLoadingFixture.calls);
    const initialRouteCalls = initialCalls.filter((call) => new URL(call, 'http://fixture').pathname === '/internal/v1/model-routes');
    const initialKeyCalls = initialCalls.filter((call) => new URL(call, 'http://fixture').pathname === '/internal/v1/keys');
    assert.equal(initialRouteCalls.length, 1, 'route list must have a one-request critical path');
    assert.equal(initialKeyCalls.length, 0, 'credential inventory must stay off the initial route-list path');

    await page.locator('details.create-resource > summary').click();
    await page.waitForFunction(() => window.routeLoadingFixture.calls.some((call) => {
      const url = new URL(call, location.origin);
      return url.pathname === '/internal/v1/keys' && !url.searchParams.has('key_id');
    }));
    let openedCalls = await page.evaluate(() => window.routeLoadingFixture.calls);
    assert.equal(openedCalls.filter((call) => new URL(call, 'http://fixture').pathname === '/internal/v1/keys').length, 1,
      'opening create performs one authoritative credential inventory read');

    const credentialField = page.locator('details.create-resource .multi-combobox').filter({
      has: page.getByRole('combobox', { name: 'Grant to specific credentials', exact: true }),
    });
    await page.evaluate(() => { document.documentElement.dataset.theme = 'light'; });
    await credentialField.getByRole('combobox').fill('00000000-0000-4000-8000-000000000001');
    await credentialField.getByRole('alert').waitFor();
    openedCalls = await page.evaluate(() => window.routeLoadingFixture.calls);
    const exactCalls = openedCalls.filter((call) => {
      const url = new URL(call, 'http://fixture');
      return url.pathname === '/internal/v1/keys' && url.searchParams.has('key_id');
    });
    assert.equal(exactCalls.length, 1, 'a UUID query in the opened form performs one bounded exact lookup');
    const colors = await credentialField.evaluate((field) => {
      const textReference = document.createElement('span');
      textReference.style.color = 'var(--picker-text)';
      const actionReference = document.createElement('button');
      actionReference.className = 'secondary';
      field.append(textReference, actionReference);
      const semanticText = getComputedStyle(textReference).color;
      const semanticAction = getComputedStyle(actionReference).color;
      textReference.remove();
      actionReference.remove();
      return {
        semanticText,
        semanticAction,
        alert: getComputedStyle(field.querySelector<HTMLElement>('[role="alert"]')!).color,
        retry: getComputedStyle(field.querySelector<HTMLElement>('button.secondary')!).color,
      };
    });
    assert.equal(colors.alert, colors.semanticText, 'light-theme error uses the readable picker text token');
    assert.equal(colors.retry, colors.semanticAction, 'light-theme retry uses the existing secondary-action token');
  } finally {
    await browser.close();
    await server.close();
  }
});
