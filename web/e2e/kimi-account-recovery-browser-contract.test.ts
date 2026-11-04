import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { mkdir } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

test('Kimi renewal evidence and quota lock recovery preserve uncertainty and separate paid confirmation', { timeout: 90_000 }, async () => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    return test.skip('Chromium required');
  }
  const root = fileURLToPath(new URL('..', import.meta.url));
  const server = await createServer({ root, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const base = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    const errors: string[] = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.route('**/*', route => new URL(route.request().url()).origin === base ? route.continue() : route.abort());
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'zh-CN'));
    const artifacts = join(root, 'e2e-artifacts', 'upstream-quota');
    await mkdir(artifacts, { recursive: true });
    const open = async (mode: string) => {
      await page.goto(`${base}/e2e/fixtures/upstream-quota.html?mode=${mode}`);
      await page.getByRole('button', { name: '查看额度', exact: true }).click();
      await page.getByRole('button', { name: '上次操作详情', exact: true }).waitFor();
      await page.waitForFunction(() => !document.querySelector<HTMLButtonElement>('.upstream-quota-reset-action button')?.disabled);
    };
    await open('recover');
    for (const [account, reason] of [['0142', /访问令牌已过期.*网络读取失败.*结果待核实.*自动续期已暂停/], ['7021', /访问令牌已过期.*未能保存.*结果待核实/]] as const) {
      const status = page.locator(`[data-account-state="${account}"] .status`);
      assert.equal(await status.innerText(), '续期结果待核对');
      await status.focus();
      await page.getByRole('tooltip').filter({ hasText: reason }).waitFor();
      assert.match(await page.getByRole('tooltip').innerText(), reason);
      await page.getByRole('button', { name: '刷新额度', exact: true }).focus();
      await page.getByRole('tooltip').waitFor({ state: 'hidden' });
    }
    assert.doesNotMatch(await page.locator('[data-account-state="0142"]').innerText(), /授权已失效|授权已过期/);
    assert.match(await page.locator('[data-account-state="revoked"]').innerText(), /授权已失效，请重新登录/);
    assert.match(await page.locator('[data-account-state="missing"]').innerText(), /未知/);
    assert.equal(await page.getByRole('checkbox').count(), 0);
    assert.deepEqual(await page.evaluate(() => [window.quotaPrepares, window.quotaConfirms, window.quotaWrites]), [0, 0, 0]);
    for (const [theme, width] of [['light', 1440], ['dark', 390]] as const) {
      await page.evaluate(value => { document.documentElement.dataset.theme = value; }, theme);
      await page.setViewportSize({ width, height: 1000 });
      assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth));
      await page.screenshot({ path: join(artifacts, `kimi-account-recovery-${theme}-${width}.png`), fullPage: true });
    }
    await page.getByRole('button', { name: '核对并恢复', exact: true }).click();
    await page.getByText('已恢复操作，上次重置结果仍待核对。', { exact: true }).waitFor();
    await page.getByRole('button', { name: '上次操作详情', exact: true }).click();
    await page.getByText(/本次操作锁定已解除，上次重置结果仍未知/).waitFor();
    assert.equal(await page.getByText(/上次操作已核对/).count(), 0);
    assert.deepEqual(await page.evaluate(() => [window.quotaPrepares, window.quotaConfirms, window.quotaWrites]), [0, 0, 1]);
    await page.screenshot({ path: join(artifacts, 'kimi-account-recovery-released.png'), fullPage: true });
    await page.getByRole('button', { name: '准备新的重置', exact: true }).click();
    await page.getByRole('button', { name: '重置上游额度', exact: true }).click();
    const dialog = page.getByRole('dialog');
    await dialog.waitFor();
    assert.deepEqual(await page.evaluate(() => [window.quotaPrepares, window.quotaConfirms]), [1, 0]);
    await dialog.getByRole('button', { name: '确认继续', exact: true }).click();
    await page.getByText(/重置已被接受，但最新额度暂未读取成功/).waitFor();
    assert.equal(await page.evaluate(() => window.quotaConfirms), 1);
    for (const [mode, message] of [
      ['recover-stale', /操作记录已变化/],
      ['recover-forbidden', /当前凭据不能解除锁定/],
      ['recover-failed', /核对或解除锁定未能确认/],
    ] as const) {
      await open(mode);
      await page.getByRole('button', { name: '核对并恢复', exact: true }).click();
      await page.getByRole('alert').getByText(message).waitFor();
      assert.equal(await page.getByRole('button', { name: '准备新的重置', exact: true }).count(), 0);
      assert.equal(await page.evaluate(() => window.quotaConfirms), 0);
      if (mode === 'recover-stale') {
        assert.equal(await page.getByRole('button', { name: '核对并恢复', exact: true }).isDisabled(), true);
        await page.getByRole('button', { name: '检查操作状态', exact: true }).click();
        await page.getByRole('alert').waitFor({ state: 'hidden' });
      }
    }
    await open('recover-tenant');
    assert.equal(await page.getByRole('button', { name: '核对并恢复', exact: true }).count(), 0);
    await open('recover-released');
    await page.getByText('已恢复操作，上次重置结果仍待核对。', { exact: true }).waitFor();
    await page.getByRole('button', { name: '上次操作详情', exact: true }).click();
    await page.getByText(/本次操作锁定已解除，上次重置结果仍未知/).waitFor();
    assert.deepEqual(await page.evaluate(() => [window.quotaPrepares, window.quotaConfirms, window.quotaWrites]), [0, 0, 0]);
    assert.deepEqual(errors, []);
  } finally { await browser.close(); await server.close(); }
});
