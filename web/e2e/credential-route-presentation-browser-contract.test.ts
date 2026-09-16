import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { mkdir } from 'node:fs/promises';
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

test('compact preview preserves exact scope and same-name source identities', { timeout: 60000 }, async () => {
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer!.address(); assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  const shots = fileURLToPath(new URL('../e2e-artifacts/upstream-availability/credential-compact', import.meta.url));
  await mkdir(shots, { recursive: true });
  try {
    for (const locale of ['zh-CN', 'en']) {
      const page = await browser.newPage();
      await page.addInitScript(value => localStorage.setItem('mtc-locale', value), locale);
      await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/credential-route-presentation.html?grouped`);
      const preview = page.locator('.credential-route-preview');
      await preview.getByText(/primary-account-with-long-name/).first().waitFor();
      const row = (model: string) => preview.getByRole('listitem').filter({ has: page.getByText(model, { exact: true }) });
      assert.equal(await preview.getByRole('listitem').count(), 10, 'scope, availability, source IDs and missing route identities all remain distinct');
      assert.equal(await row('Beta').getByText('Gamma', { exact: true }).count(), 1, 'identical scope and direct source share one compact preview group');
      assert.equal(await row('Group alpha').getByText('Group beta', { exact: true }).count(), 0, 'same-name groups are not the same source');
      assert.match(await row('Alpha').innerText(), /Same group \/ Same group/);
      assert.match(await row('Alpha').innerText(), locale === 'en' ? /Direct grant/ : /直接授权/);
      assert.match(await row('Shared').innerText(), locale === 'en' ? /Shared candidates/ : /共享候选/);
      assert.match(await row('Unknown scope').innerText(), locale === 'en' ? /unconfirmed/ : /未确认/);
      assert.match(await row('Disabled model').innerText(), locale === 'en' ? /Currently unavailable/ : /当前不可用/);
      const toggle = page.getByRole('button', { name: /单独授权模型|Individual model grants/ });
      assert.equal(await toggle.getAttribute('aria-expanded'), 'false');
      await page.locator('.credential-route-selection-summary').waitFor();
      const original = await page.getByTestId('grant-ids').textContent();
      for (const width of [390, 1440]) {
        await page.setViewportSize({ width, height: 1000 });
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false);
        await page.screenshot({ path: `${shots}/${locale}-${width}.png`, fullPage: true });
      }
      await toggle.focus(); await page.keyboard.press('Enter');
      const groupInput = page.getByRole('combobox', { name: locale === 'en' ? 'Route groups' : '路由组', exact: true });
      await groupInput.click();
      await page.getByText(locale === 'en' ? 'All route groups are already selected' : '所有路由组均已选择', { exact: true }).waitFor();
      await groupInput.fill('missing-search');
      await page.getByText(locale === 'en' ? 'No matches' : '没有匹配项', { exact: true }).waitFor();
      await groupInput.press('Escape');
      assert.equal(await page.getByTestId('grant-ids').textContent(), original, 'disclosure and search never change grant IDs');
      await page.close();
    }
  } finally { await browser.close(); await server.close(); }
});
