import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer } from './support/isolated-vite-server.js';

type FixtureWindow = Window & {
  healthProbeFixture: { writes: { body: { prompt: string }; release: (fail?: boolean) => void }[] };
};

test('old probe saves cannot change settings or unlock a save in a new account, tenant or token', { timeout: 120_000 }, async () => {
  const server = await createIsolatedFixtureServer({
    root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent',
    server: { host: '127.0.0.1', port: 0 },
  });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    for (const scope of ['account', 'tenant', 'token']) {
      for (const fail of [false, true]) {
        const page = await browser.newPage();
        await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
        await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/health-probe-settings.html`);
        const prompt = page.locator('textarea');
        const save = page.getByRole('button', { name: 'Save probe settings', exact: true });
        await prompt.fill('old account prompt');
        await save.click();
        await page.waitForFunction(() => (window as FixtureWindow).healthProbeFixture.writes.length === 1);
        await page.getByRole('button', { name: `Switch ${scope}`, exact: true }).click();
        await page.waitForFunction(() => {
          const textarea = document.querySelector('textarea');
          return textarea && textarea.value !== 'old account prompt' && !textarea.disabled;
        });
        await prompt.fill('new account prompt');
        await save.click();
        await page.waitForFunction(() => (window as FixtureWindow).healthProbeFixture.writes.length === 2);
        await page.evaluate(failed => (window as FixtureWindow).healthProbeFixture.writes[0].release(failed), fail);
        // Let the resolved fetch and React commit finish before asserting.
        await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
        assert.equal(await prompt.inputValue(), 'new account prompt');
        assert.equal(await save.isDisabled(), true, 'old finally must not unlock the current save');
        assert.equal(await page.getByRole('alert').count(), 0);
        assert.equal(await page.getByRole('status').count(), 0, 'old success must not mark new settings saved');
        assert.equal(await page.evaluate(() => (window as FixtureWindow).healthProbeFixture.writes[1].body.prompt), 'new account prompt');
        await page.evaluate(() => (window as FixtureWindow).healthProbeFixture.writes[1].release());
        await page.getByRole('status').waitFor();
        assert.equal(await page.getByRole('status').textContent(), 'Saved');
        assert.equal(await save.isEnabled(), true);
        await page.close();
      }
    }
  } finally {
    await browser.close();
    await server.close();
  }
});
