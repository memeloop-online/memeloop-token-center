import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer as createServer } from './support/isolated-vite-server.js';

test('real form composition keeps OAuth proxy optional and route drafts across disclosure', { timeout: 60_000 }, async () => {
  const root = fileURLToPath(new URL('..', import.meta.url));
  const server = await createServer({ root, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const origin = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    await page.clock.setFixedTime(new Date('2026-09-13T07:00:00Z'));
    const errors: string[] = []; page.on('pageerror', error => errors.push(error.message));
    await page.route('**/*', route => new URL(route.request().url()).origin === origin && !new URL(route.request().url()).pathname.startsWith('/internal/') ? route.continue() : route.abort());
    await page.addInitScript(() => { if (!localStorage.getItem('mtc-locale')) localStorage.setItem('mtc-locale', 'zh-CN'); });
    await page.goto(`${origin}/e2e/fixtures/form-journey.html`);
    await page.getByRole('button', { name: '新增上游', exact: true }).click();
    await page.getByRole('button', { name: '账户授权', exact: true }).click();
    assert.equal(await page.getByRole('button', { name: '开始登录', exact: true }).isEnabled(), true, 'direct network environments can start Codex login');
    await page.getByRole('checkbox', { name: '使用账号网络代理', exact: true }).check();
    assert.equal(await page.getByRole('button', { name: '开始登录', exact: true }).isEnabled(), false, 'enabling the proxy waits for a valid address');
    await page.locator('.authorization-form input[type="password"]').fill('socks5://100.64.0.20:1080');
    assert.equal(await page.getByRole('button', { name: '开始登录', exact: true }).isEnabled(), false, 'local DNS proxy is rejected');
    await page.locator('.authorization-form input[type="password"]').fill('socks5h://100.64.0.20:1080');
    assert.equal(await page.getByRole('button', { name: '开始登录', exact: true }).isEnabled(), true);
    // Never click login or any quota action. All writes are forbidden in fixture.
    const artifacts = `${root}/e2e-artifacts/form-journey`; await mkdir(artifacts, { recursive: true });
    for (const [theme, width] of [['light', 390], ['dark', 1440]] as const) {
      await page.setViewportSize({ width, height: 1000 });
      await page.evaluate(theme => { document.documentElement.dataset.theme = theme; }, theme);
      assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth));
      await page.screenshot({ path: `${artifacts}/providers-${theme}-${width}.png`, fullPage: true });
    }
    assert.equal(await page.evaluate(() => window.formJourneyWrites), 0);
    await page.getByRole('button', { name: 'API 凭据', exact: true }).click();
    await page.getByRole('button', { name: /高级网络/ }).click();
    await page.screenshot({ path: `${artifacts}/providers-direct-dark-1440.png`, fullPage: true });
    await page.goto(`${origin}/e2e/fixtures/form-journey.html?view=routes`);
    await page.locator('.create-journey [data-workspace-toggle]').click();
    const priority = page.getByRole('button', { name: '路由优先级', exact: true });
    await priority.click();
    const input = page.locator('.route-form-sections input[type="number"]');
    await input.fill('7'); await priority.click(); await priority.click();
    assert.equal(await input.inputValue(), '7');
    const routeToggle = page.locator('.create-journey [data-workspace-toggle]');
    await routeToggle.click(); await routeToggle.click();
    assert.equal(await input.inputValue(), '7', 'closing the workspace preserves its draft');
    await page.getByRole('region', { name: '配置预览' }).waitFor();
    const sourceSection = page.locator('.route-form-sections > .mtc-form-section').nth(1);
    const accessSection = page.locator('.route-form-sections > .form-journey-disclosure');
    const sourceBounds = await sourceSection.boundingBox();
    const accessBounds = await accessSection.boundingBox();
    assert.ok(sourceBounds && accessBounds && accessBounds.y >= sourceBounds.y + sourceBounds.height, 'authorization follows the complete source/model step, including on desktop');
    await page.screenshot({ path: `${artifacts}/routes-dark-1440.png`, fullPage: true });
    assert.equal(await page.evaluate(() => window.formJourneyWrites), 0);
    await page.goto(`${origin}/e2e/fixtures/operator-credential-workspace.html?scenario=client-form`);
    await page.locator('.create-journey [data-workspace-toggle]').click();
    await page.locator('.create-journey').getByRole('button', { name: '用量与预算', exact: true }).click();
    const budgets = page.getByRole('button', { name: '可选用量限制', exact: true });
    assert.equal(await budgets.getAttribute('aria-expanded'), 'false');
    await budgets.click();
    const daily = page.locator('#root_policy_daily_budget');
    await daily.fill('12.5'); await budgets.click(); await budgets.click();
    assert.equal(await daily.inputValue(), '12.5');
    await page.locator('#root_alias').fill('Unsaved client draft');
    await page.locator('[data-change-locale]').click();
    await page.getByRole('button', { name: 'Usage and budget', exact: true }).waitFor();
    assert.equal(await page.locator('#root_alias').inputValue(), 'Unsaved client draft');
    assert.equal(await daily.inputValue(), '12.5', 'locale changes preserve the budget draft');
    const credentialToggle = page.locator('.create-journey [data-workspace-toggle]');
    await credentialToggle.click(); await credentialToggle.click();
    assert.equal(await page.locator('#root_alias').inputValue(), 'Unsaved client draft');
    assert.equal(await daily.inputValue(), '12.5', 'closing and reopening preserves the create draft');
    await page.setViewportSize({ width: 390, height: 1000 });
    await page.screenshot({ path: `${artifacts}/credentials-dark-390.png`, fullPage: true });
    assert.equal(await page.evaluate(() => window.credentialFixture.requests.filter(request => request.method !== 'GET').length), 0);
    for (const locale of ['zh-CN', 'en']) for (const view of ['routes', 'credentials']) {
      await page.evaluate(locale => localStorage.setItem('mtc-locale', locale), locale);
      await page.goto(view === 'routes' ? `${origin}/e2e/fixtures/form-journey.html?view=routes` : `${origin}/e2e/fixtures/operator-credential-workspace.html?scenario=client-form`);
      const create = page.locator('.create-journey');
      const toggle = create.locator(':scope > .journey-heading [data-workspace-toggle]');
      await toggle.click();
      assert.match(await toggle.innerText(), locale === 'en' ? /Close/ : /关闭/);
      assert.equal(await create.evaluate(element => element.classList.contains('panel')), false, 'editor directly occupies the main area, without an outer card');
      assert.ok(await create.locator('fieldset').count() > 0);
      assert.ok(await create.locator('fieldset').evaluateAll(elements => elements.every(element => getComputedStyle(element).borderTopWidth === '0px')), 'form groups use headings, not nested frames');
      for (const theme of ['light', 'dark']) for (const width of [320, 390, 1440]) {
        await page.evaluate(theme => { document.documentElement.dataset.theme = theme; }, theme);
        await page.setViewportSize({ width, height: 1000 });
        assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth));
        assert.ok(await create.evaluate(element => element.scrollWidth <= element.clientWidth), 'form contents fit their work surface');
        await page.screenshot({ path: `${artifacts}/${view}-${locale}-${theme}-${width}-round3.png`, fullPage: true });
      }
      await toggle.click();
      if (view === 'credentials') {
        for (const theme of ['light', 'dark']) for (const width of [320, 390, 1440]) {
          await page.evaluate(theme => { document.documentElement.dataset.theme = theme; }, theme);
          await page.setViewportSize({ width, height: 1000 });
          await page.screenshot({ path: `${artifacts}/credential-list-${locale}-${theme}-${width}-round3.png`, fullPage: true });
        }
        assert.equal(await page.getByRole('menuitem', { name: locale === 'en' ? 'Rename' : '修改别名', exact: true }).isVisible(), false);
        await page.getByRole('button', { name: locale === 'en' ? 'More actions' : '更多操作', exact: true }).click();
        assert.equal(await page.getByRole('menuitem', { name: locale === 'en' ? 'Rename' : '修改别名', exact: true }).isVisible(), true);
        for (const width of [320, 1440]) {
          await page.setViewportSize({ width, height: 1000 });
          await page.getByRole('menu').waitFor();
          await page.screenshot({ path: `${artifacts}/credential-menu-${locale}-dark-${width}-round3.png`, fullPage: true });
        }
        assert.equal(await page.evaluate(() => window.credentialFixture.requests.filter(request => request.method !== 'GET').length), 0);
      } else assert.equal(await page.evaluate(() => window.formJourneyWrites), 0);
    }
    assert.deepEqual(errors, []);
  } finally { await browser.close(); await server.close(); }
});
