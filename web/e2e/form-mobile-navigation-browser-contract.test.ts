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
    assert.equal(await page.locator('.provider-edit-workspace legend').filter({ hasText: '1. 上游身份与认证' }).count(), 0);
    assert.equal(await page.locator('.provider-edit-workspace .upstream-connection').evaluate(element => getComputedStyle(element).borderTopWidth), '0px');
    assert.equal(await page.locator('.provider-edit-workspace .inline-editor').evaluate(element => getComputedStyle(element).backgroundColor), 'rgba(0, 0, 0, 0)');
    assert.equal(await page.locator('.provider-edit-workspace .rjsf > button[type="submit"]').evaluate(element => element.classList.contains('fui-Button')), true);
    await page.locator('.create-journey [data-workspace-toggle]').click({ trial: true });
    const artifacts = `${root}/e2e-artifacts/provider-edit-refinement`;
    await mkdir(artifacts, { recursive: true });
    await page.screenshot({ path: `${artifacts}/provider-edit-appshell-light-320.png`, fullPage: true });
    await page.setViewportSize({ width: 1440, height: 1000 });
    assert.ok(await page.locator('.provider-edit-workspace').evaluate(element => element.getBoundingClientRect().width <= 720));
    await page.locator('.create-journey input').first().focus();
    const layout = await page.evaluate(() => ({ scrollY, headerTop: document.querySelector('.app-context-bar')!.getBoundingClientRect().top, skipBottom: document.querySelector('.app-skip-link')!.getBoundingClientRect().bottom, focusTop: document.activeElement!.getBoundingClientRect().top }));
    assert.ok(layout.skipBottom <= 0, 'unfocused skip link remains outside viewport');
    assert.ok(layout.focusTop >= 64, 'focused editor input is below the sticky header');
    await page.screenshot({ path: `${artifacts}/provider-edit-desktop-viewport.png`, fullPage: false });
    await page.getByRole('button', { name: '配置网络代理', exact: true }).click();
    const proxy = page.locator('.upstream-proxy-editor input');
    await proxy.fill('socks5://10.0.0.10:1080');
    assert.equal(await page.getByRole('button', { name: '保存网络代理', exact: true }).isEnabled(), false);
    await proxy.fill('socks5h://10.0.0.10:1080');
    assert.equal(await page.getByRole('button', { name: '保存网络代理', exact: true }).isEnabled(), true);
    // Only inspect the draft. Never activate either proxy or provider save.
    for (const width of [320, 1440]) {
      await page.setViewportSize({ width, height: 1000 });
      await page.evaluate(() => { document.documentElement.dataset.theme = 'dark'; });
      await page.waitForFunction(() => matchMedia('(min-width: 901px)').matches || document.querySelector('.app-sidebar')!.getBoundingClientRect().right <= 0);
      await page.screenshot({ path: `${artifacts}/provider-proxy-dark-${width}.png`, fullPage: true });
    }
    console.log('desktop viewport geometry', layout);
    assert.equal(await page.evaluate(() => window.formJourneyWrites), 0);
  } finally { await browser.close(); await server.close(); }
});
