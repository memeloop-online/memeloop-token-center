import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer } from './support/isolated-vite-server.js';
import { fixtureAssets } from './support/fixture-assets.js';

test('mobile navigation closes before editing and uses an opaque surface', { timeout: 60_000 }, async () => {
  const root = fileURLToPath(new URL('..', import.meta.url));
  const server = await createIsolatedFixtureServer({ root, configFile: false, plugins: [fixtureAssets()], logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const origin = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 320, height: 1000 } });
    await page.route('**/*', route => {
      const request = route.request(), url = new URL(request.url());
      return url.origin === origin && request.method() === 'GET' && !url.pathname.startsWith('/internal/') ? route.continue() : route.abort();
    });
    await page.addInitScript(() => { localStorage.setItem('mtc-locale', 'zh-CN'); document.documentElement.dataset.theme = 'light'; });
    await page.goto(`${origin}/e2e/fixtures/form-journey.html?view=routes&workflows=1`);
    await page.getByRole('button', { name: '打开导航', exact: true }).click();
    await page.evaluate(() => { document.documentElement.dataset.theme = 'light'; });
    assert.equal(await page.locator('.app-sidebar').evaluate(element => getComputedStyle(element).backgroundColor), 'rgb(248, 251, 249)');
    await page.getByRole('link', { name: '上游服务', exact: true }).click();
    await page.waitForFunction(() => document.querySelector('.app-sidebar')!.getBoundingClientRect().right <= 0);
    assert.equal(await page.locator('.app-sidebar').evaluate(element => getComputedStyle(element).visibility), 'hidden');
    assert.equal(await page.locator('.app-stage').evaluate(element => (element as HTMLElement).inert), false);
    await page.getByText('账号设置与授权操作', { exact: true }).click();
    await page.getByRole('button', { name: '编辑', exact: true }).click();
    assert.equal(await page.getByRole('heading', { name: '编辑 研发订阅', exact: true }).count(), 1);
    assert.equal(await page.locator('.create-journey input:not([type="hidden"])').evaluateAll(elements => elements.filter(element => (element as HTMLInputElement).value.includes('chatgpt.com')).length), 0);
    assert.equal(await page.locator('.create-journey code').filter({ hasText: 'https://chatgpt.com/backend-api/codex' }).count(), 1);
    await page.getByRole('button', { name: '配置网络代理', exact: true }).click({ trial: true });
    await page.locator('.create-journey [data-workspace-toggle]').click({ trial: true });
    const artifacts = `${root}/e2e-artifacts/form-workspace-round3`;
    await mkdir(artifacts, { recursive: true });
    await page.screenshot({ path: `${artifacts}/provider-edit-appshell-light-320.png`, fullPage: true });
    await page.setViewportSize({ width: 1440, height: 1000 });
    await page.locator('.create-journey input').first().focus();
    const layout = await page.evaluate(() => ({ scrollY, headerTop: document.querySelector('.app-context-bar')!.getBoundingClientRect().top, skipBottom: document.querySelector('.app-skip-link')!.getBoundingClientRect().bottom, focusTop: document.activeElement!.getBoundingClientRect().top }));
    assert.ok(layout.skipBottom <= 0, 'unfocused skip link remains outside viewport');
    assert.ok(layout.focusTop >= 64, 'focused editor input is below the sticky header');
    await page.screenshot({ path: `${artifacts}/provider-edit-desktop-viewport.png`, fullPage: false });
    console.log('desktop viewport geometry', layout);
    assert.equal(await page.evaluate(() => window.formJourneyWrites), 0);
  } finally { await browser.close(); await server.close(); }
});
