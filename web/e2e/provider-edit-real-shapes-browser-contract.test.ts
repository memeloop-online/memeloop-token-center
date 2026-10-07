import assert from 'node:assert/strict';
import { editProviderAccount } from './support/provider-account-navigation.js';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer } from './support/isolated-vite-server.js';
import { providerEditShape } from './fixtures/provider-edit-shapes.js';

test('real Codex config shapes localize transport controls and preserve advanced edits and unknown data', { timeout: 90_000 }, async () => {
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
    for (const shape of ['csil', 'lindongwu', 'retry-only']) {
      await page.goto(`${origin}/e2e/fixtures/form-journey.html?workflows&proxy-workflow&provider-shape=${shape}`);
      await editProviderAccount(page);
      const workspace = page.locator('.provider-edit-workspace');
      const original = providerEditShape(shape)!.config;
      if (shape === 'retry-only') {
        await workspace.locator('.rjsf > button[type="submit"]').click();
        assert.deepEqual((await page.evaluate(() => window.formJourneyLastProviderWrite))?.config,
          { base_url: 'https://chatgpt.com/backend-api/codex', ...original }, 'opening and saving without edits does not materialize defaults');
        await editProviderAccount(page);
      }
      const advanced = workspace.getByRole('button', { name: '高级网络与用量设置', exact: true });
      assert.equal(await advanced.getAttribute('aria-expanded'), 'false');
      const proxy = workspace.getByRole('button', { name: '配置网络代理', exact: true });
      await workspace.locator('.provider-proxy-value input').waitFor();
      assert.ok(await proxy.evaluate(element => element.getBoundingClientRect().bottom < innerHeight), 'proxy action is available in the initial desktop viewport');
      assert.doesNotMatch(await workspace.innerText(), /reservation_token_bounds|Conservative token|network_scope|代理标识|synthetic-diagnostic-fingerprint/);
      assert.equal(await workspace.locator('.provider-readable-proxy').getByRole('button', { name: '配置网络代理', exact: true }).count(), 1, 'editing and copying sit beside the single authorized proxy value');
      const retry = workspace.getByRole('button', { name: '配置超时与重试', exact: true });
      assert.equal(await retry.getAttribute('aria-expanded'), 'false');
      await retry.focus(); await page.keyboard.press('Enter');
      await workspace.getByLabel('连接尝试次数').waitFor();
      assert.doesNotMatch(await workspace.innerText(), /Pre-delivery|Maximum inactivity|One absolute budget/);
      await workspace.getByText('建立连接的等待时限，必须小于请求总超时。', { exact: true }).waitFor();
      await workspace.getByText('运行时传输策略', { exact: true }).waitFor();
      await workspace.getByText('单位见各字段；这些设置影响该账号的所有请求。', { exact: true }).waitFor();
      for (const title of ['最大 SSE 事件（字节）', '单个网络分片最大帧数据量（字节）', '终止事件最大暂存量（字节）',
        '内存排队超时（毫秒）', '调度最大并发数', '调度队列容量', '调度排队超时（毫秒）']) {
        await workspace.getByLabel(title, { exact: true }).waitFor();
      }
      for (const [title, description] of [
        ['Chat 控制策略', '默认策略（provider_default）会校验并移除不受支持的 Chat 采样和输出限制参数；上游使用自身默认值，不保证请求指定的采样设置或词元上限。显式选择严格策略（strict）时，仅接受中性的采样值，并拒绝输出限制提示。额度预留仍使用可信的模型上限。'],
        ['Responses 输出限制', '默认策略（provider_default）会校验并移除客户端的输出限制提示；上游使用自身默认值，不强制执行请求指定的词元上限。由于 Codex OAuth 无法保证该限制，显式选择严格策略（strict）时会拒绝任何输出限制提示。额度始终按可信的模型上限预留，并按观测到的实际用量结算。'],
      ]) {
        await workspace.getByLabel(title, { exact: true }).waitFor();
        await workspace.getByText(description, { exact: true }).waitFor();
      }
      assert.doesNotMatch(await workspace.innerText(), /Maximum SSE|Maximum framed|Maximum terminal|Memory queue timeout|dispatch_max_in_flight|dispatch_max_queued|dispatch_queue_timeout_millis|Chat controls|Responses output limits|Provider default validates/);
      await retry.click();
      await advanced.focus(); await page.keyboard.press('Enter');
      await workspace.getByLabel('网络访问范围', { exact: false }).waitFor();
      const model = shape === 'lindongwu' ? 'gpt-5.5' : 'gpt-5.3-codex-spark';
      const bound = workspace.getByRole('textbox', { name: model, exact: true });
      await bound.fill('72000');
      assert.equal(await workspace.getByRole('button', { name: `删除字段：${model}`, exact: true }).count(), 1);
      assert.equal(await workspace.getByRole('textbox', { name: `字段名称：${model}`, exact: true }).count(), 1);
      const key = workspace.getByRole('textbox', { name: `字段名称：${model}`, exact: true });
      await key.fill(`${model}-renamed`); await key.press('Tab');
      assert.equal(await workspace.getByRole('textbox', { name: `${model}-renamed`, exact: true }).inputValue(), '72000', 'renaming a model key retains its reservation');
      const renamed = workspace.getByRole('textbox', { name: `字段名称：${model}-renamed`, exact: true });
      await renamed.fill(model); await renamed.press('Tab');
      assert.equal(await workspace.locator('button').evaluateAll(buttons => buttons.filter(button => !button.textContent?.trim() && !button.getAttribute('aria-label') && !button.getAttribute('aria-labelledby')).length), 0);
      for (const width of [390, 1440]) for (const theme of ['light', 'dark']) {
        await page.setViewportSize({ width, height: 1000 });
        await page.evaluate(theme => { document.documentElement.dataset.theme = theme; }, theme);
        await page.waitForFunction(() => matchMedia('(min-width: 901px)').matches || document.querySelector('.app-sidebar')!.getBoundingClientRect().right <= 0);
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
      }
      await advanced.click(); await advanced.click();
      assert.equal(await bound.inputValue(), '72000', 'collapsing retains advanced drafts');
      // Local in-memory save only. Reopening proves all other model entries and
      // a field unknown to the plugin schema survived the actual page submission.
      await workspace.locator('.rjsf > button[type="submit"]').click();
      assert.deepEqual(await page.evaluate(() => window.formJourneyLastProviderWrite), {
        name: 'synthetic.automation.account@example.invalid', tenant_external_id: 'fixture', expected_updated_at: shape === 'retry-only' ? 2 : 1,
        config: { base_url: 'https://chatgpt.com/backend-api/codex', ...original,
          reservation_token_bounds: { ...original.reservation_token_bounds, [model]: 72000 } },
      }, 'only the edited reservation changes; absent fields, zero queue capacity and policy enum values are retained');
      await editProviderAccount(page);
      await advanced.click();
      assert.equal(await bound.inputValue(), '72000');
      assert.equal(await workspace.getByRole('textbox', { name: 'gpt-6-astra', exact: true }).inputValue(), '64000');
      assert.equal(await workspace.getByRole('textbox', { name: 'future_setting', exact: true }).inputValue(), 'preserve-unknown-field');
      assert.equal(await page.evaluate(() => window.formJourneyWrites), shape === 'retry-only' ? 2 : 1);
      await page.locator('.create-journey [data-workspace-toggle]').click();
    }
    assert.deepEqual(errors, []);
  } finally { await browser.close(); await server.close(); }
});
