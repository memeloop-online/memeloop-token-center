import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

test('route models lead compact chips, scope remains visible, details do not change grants', { timeout: 30000 }, async context => {
  const executablePath = process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH ?? chromium.executablePath();
  if (!existsSync(executablePath)) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    return context.skip('Chromium required');
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer!.address(); assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ executablePath, headless: true });
  try {
    const page = await browser.newPage({ hasTouch: true });
    const errors: string[] = []; page.on('pageerror', error => errors.push(error.message));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/credential-route-presentation.html`);
    await page.getByRole('button', { name: /单独授权模型|Individual model grants/ }).click();
    const chips = page.locator('.selection-chip-hierarchy');
    await chips.filter({ hasText: 'primary-account-with-long-name@example.test' }).waitFor();
    const initial = await page.getByTestId('grant-ids').textContent();
    for (const [width, theme] of [[390, 'light'], [1440, 'dark']] as const) {
      await page.setViewportSize({ width, height: 1000 });
      await page.evaluate(theme => { document.documentElement.dataset.theme = theme; }, theme);
      assert.equal(await chips.count(), 2);
      assert.equal(await chips.first().locator(':scope > span').textContent(), 'Voice model');
      assert.match(await chips.nth(1).locator('small').textContent() ?? '', /共享候选 · 2 个账号/);
      for (const action of ['pointer', 'keyboard', 'touch']) {
        await page.getByRole('heading', { name: '路由展示' }).click();
        if (action === 'pointer') await chips.nth(1).hover();
        if (action === 'keyboard') { await chips.first().focus(); await page.keyboard.press('Tab'); await page.keyboard.press('Tab'); }
        if (action === 'touch') await chips.nth(1).tap();
        const tip = page.getByRole('tooltip').filter({ hasText: '路由 ID: shared-route-id' });
        await tip.waitFor();
        assert.match(await tip.textContent() ?? '', /primary-account-with-long-name@example.test/);
        assert.match(await tip.textContent() ?? '', /secondary-account@example.test/);
        assert.match(await tip.textContent() ?? '', /提供商: Fixture provider/);
        const bounds = await tip.boundingBox(); assert.ok(bounds);
        assert.ok(bounds.x >= -1 && bounds.x + bounds.width <= width + 1, 'visible detail must fit the viewport');
        assert.equal(await page.getByTestId('grant-ids').textContent(), initial);
        await page.keyboard.press('Escape'); await tip.waitFor({ state: 'hidden' });
      }
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false);
    }
    assert.deepEqual(errors, []);
  } finally { await browser.close(); await server.close(); }
});
