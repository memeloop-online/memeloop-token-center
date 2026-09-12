import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { mkdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

test('create and rotate keep provider secrets masked across keyboard, mobile and schema variants', { timeout: 120_000 }, async (context) => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    context.skip('Chromium required'); return;
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  const artifacts = fileURLToPath(new URL('../e2e-artifacts/secret-inputs/', import.meta.url));
  await mkdir(artifacts, { recursive: true });
  try {
    for (const variant of ['api', 'oauth', 'plugin']) for (const locale of ['en', 'zh-CN']) {
      const page = await browser.newPage();
      const failures: string[] = [];
      page.on('pageerror', () => failures.push('runtime error'));
      page.on('console', (message) => { if (/synthetic|must-not-prefill|must-not-suggest/.test(message.text())) failures.push('value logged'); });
      await page.addInitScript((value) => localStorage.setItem('mtc-locale', value), locale);
      await page.route('**/*', async (route) => {
        if (new URL(route.request().url()).hostname !== '127.0.0.1') { failures.push('external request'); await route.abort(); return; }
        await route.continue();
      });
      await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/secret-credential-inputs.html?variant=${variant}`);
      await page.locator('.schema-secret-field input').first().waitFor();
      for (const [theme, width] of [['light', 1440], ['dark', 390]] as const) {
        await page.setViewportSize({ width, height: 900 });
        await page.evaluate((value) => { document.documentElement.dataset.theme = value; }, theme);
        await page.getByRole('button', { name: 'Reopen forms', exact: true }).click();
        for (const name of ['Create credential', 'Rotate credential']) {
          const form = page.getByRole('region', { name, exact: true });
          const inputs = form.locator('.schema-secret-field input');
          assert.ok(await inputs.count() >= 2, `${variant}: secret strings and opaque/ref fields rendered`);
          for (const input of await inputs.all()) {
            assert.equal(await input.getAttribute('type'), 'password');
            assert.equal(await input.inputValue() === '', true, 'existing/default secret must not be prefilled');
            assert.equal(await input.getAttribute('autocomplete'), 'new-password');
            const synthetic = (await input.getAttribute('id'))?.endsWith('adapter_state') ? '{"synthetic":true}' : 'synthetic-only';
            if ((await input.getAttribute('id'))?.endsWith('adapter_state')) {
              await input.fill('synthetic-invalid-json');
              assert.equal(await input.getAttribute('aria-invalid'), 'true');
              assert.equal(await form.innerText().then((text) => text.includes('synthetic-invalid-json')), false, 'JSON validation never echoes opaque state');
            }
            await input.fill(synthetic);
            const toggle = input.locator('..').getByRole('button');
            await toggle.focus(); await page.keyboard.press('Enter');
            assert.equal(await toggle.getAttribute('aria-pressed'), 'true');
            assert.equal(await input.getAttribute('type'), 'text');
            assert.equal(await input.inputValue() === synthetic, true);
            assert.equal((await input.getAttribute('value') ?? '') === '', true, 'value remains a property, never a DOM attribute');
            await page.keyboard.press('Escape');
            assert.equal(await input.getAttribute('type'), 'password');
            assert.equal(await input.inputValue() === synthetic, true, 'hiding must not clear input');
            await toggle.click();
            await page.evaluate(() => document.dispatchEvent(new Event('visibilitychange')));
            assert.equal(await input.getAttribute('type'), 'password');
            assert.equal(await input.inputValue() === synthetic, true);
            await toggle.click();
            await page.getByRole('button', { name: 'Reopen forms', exact: true }).focus();
            assert.equal(await input.getAttribute('type'), 'password', 'focus leaving remasks input');
            assert.equal(await input.inputValue() === synthetic, true);
            assert.equal(await form.innerText().then((text) => text.includes(synthetic)), false, 'no value echoed in surrounding UI');
            if (width === 390) assert.ok(await toggle.evaluate((element) => element.getBoundingClientRect().height >= 44));
          }
          await form.locator('button[type=submit]').click();
        }
        assert.equal(await page.getByRole('region', { name: 'Edit connection', exact: true }).locator('.secret-input').count(), 0, 'ordinary editing does not fetch/prefill stored credentials');
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
        await page.screenshot({ path: `${artifacts}/${variant}-${locale}-${theme}-${width}.png`, fullPage: true, mask: [page.locator('input')] });
      }
      assert.match(await page.getByRole('status').textContent() ?? '', /4$/);
      assert.deepEqual(failures, []);
      await page.close();
    }
  } finally { await browser.close(); await server.close(); }
});
