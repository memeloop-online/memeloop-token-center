import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer } from './support/isolated-vite-server.js';

test('real Codex config shapes keep proxy primary and preserve advanced edits and unknown data', { timeout: 90_000 }, async () => {
  const server = await createIsolatedFixtureServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const origin = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 1440, height: 1000 } });
    const errors: string[] = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.route('**/*', route => {
      const url = new URL(route.request().url());
      return url.origin === origin && !url.pathname.startsWith('/internal/') ? route.continue() : route.abort();
    });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'zh-CN'));
    for (const shape of ['csil', 'lindongwu']) {
      await page.goto(`${origin}/e2e/fixtures/form-journey.html?workflows&proxy-workflow&provider-shape=${shape}`);
      await page.getByRole('button', { name: '编辑', exact: true }).click();
      const workspace = page.locator('.provider-edit-workspace');
      const advanced = workspace.getByRole('button', { name: '高级配置与模型预留', exact: true });
      assert.equal(await advanced.getAttribute('aria-expanded'), 'false');
      const proxy = workspace.getByRole('button', { name: '配置网络代理', exact: true });
      assert.ok(await proxy.evaluate(element => element.getBoundingClientRect().bottom < innerHeight), 'proxy action is available in the initial desktop viewport');
      assert.doesNotMatch(await workspace.innerText(), /reservation_token_bounds|Conservative token|network_scope/);
      await advanced.focus(); await page.keyboard.press('Enter');
      await workspace.getByLabel('网络访问范围', { exact: false }).waitFor();
      const model = shape === 'csil' ? 'gpt-5.3-codex-spark' : 'gpt-5.5';
      const bound = workspace.getByRole('spinbutton', { name: model, exact: true });
      await bound.fill('72000');
      assert.equal(await workspace.getByRole('button', { name: `删除字段：${model}`, exact: true }).count(), 1);
      assert.equal(await workspace.getByRole('textbox', { name: `字段名称：${model}`, exact: true }).count(), 1);
      assert.equal(await workspace.locator('button').evaluateAll(buttons => buttons.filter(button => !button.textContent?.trim() && !button.getAttribute('aria-label') && !button.getAttribute('aria-labelledby')).length), 0);
      for (const width of [390, 1440]) for (const theme of ['light', 'dark']) {
        await page.setViewportSize({ width, height: 1000 });
        await page.evaluate(theme => { document.documentElement.dataset.theme = theme; }, theme);
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
      }
      await advanced.click(); await advanced.click();
      assert.equal(await bound.inputValue(), '72000', 'collapsing retains advanced drafts');
      // Local in-memory save only. Reopening proves all other model entries and
      // a field unknown to the plugin schema survived the actual page submission.
      await workspace.locator('.rjsf > button[type="submit"]').click();
      await page.getByRole('button', { name: '编辑', exact: true }).click();
      await advanced.click();
      assert.equal(await bound.inputValue(), '72000');
      assert.equal(await workspace.getByRole('spinbutton', { name: 'gpt-6-astra', exact: true }).inputValue(), '64000');
      assert.equal(await workspace.getByRole('textbox', { name: 'future_setting', exact: true }).inputValue(), 'preserve-unknown-field');
      assert.equal(await page.evaluate(() => window.formJourneyWrites), 1);
      await page.locator('.create-journey [data-workspace-toggle]').click();
    }
    assert.deepEqual(errors, []);
  } finally { await browser.close(); await server.close(); }
});
