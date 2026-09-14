import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer } from './support/isolated-vite-server.js';

test('independent proxy save updates concurrency metadata without dropping the provider draft', { timeout: 60_000 }, async () => {
  const root = fileURLToPath(new URL('..', import.meta.url));
  const server = await createIsolatedFixtureServer({ root, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const origin = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    const errors: string[] = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.route('**/*', route => {
      const url = new URL(route.request().url());
      return url.origin === origin && !url.pathname.startsWith('/internal/') ? route.continue() : route.abort();
    });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'zh-CN'));
    // All saves below are handled by an explicit in-memory fixture. No server
    // mutation, real proxy connection, OAuth, quota action or model request runs.
    await page.goto(`${origin}/e2e/fixtures/form-journey.html?workflows&proxy-workflow`);
    const row = page.locator('.provider-directory-row');
    await row.waitFor();
    assert.doesNotMatch(await row.innerText(), /account-native|openai-codex|credential_generation/);
    assert.match(await row.innerText(), /尚未读取/);
    assert.equal(await page.locator('.provider-detail-workspace').count(), 0);
    assert.equal(await page.locator('.provider-directory details').count(), 0);
    const artifacts = `${root}/e2e-artifacts/upstream-availability/provider-directory`;
    await mkdir(artifacts, { recursive: true });
    for (const [theme, width] of [['light', 1440], ['dark', 390]] as const) {
      await page.setViewportSize({ width, height: 1000 });
      await page.evaluate(theme => { document.documentElement.dataset.theme = theme; }, theme);
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > document.documentElement.clientWidth), false);
      await page.screenshot({ path: `${artifacts}/list-${theme}-${width}.png`, fullPage: true });
      await row.getByRole('button', { name: '查看详情', exact: true }).click();
      await page.locator('.provider-detail-workspace').waitFor();
      assert.equal(await page.locator('.provider-directory details').count(), 0);
      assert.equal(await page.evaluate(() => window.formJourneyWrites), 0);
      await page.screenshot({ path: `${artifacts}/detail-${theme}-${width}.png`, fullPage: true });
      await row.getByRole('button', { name: '收起详情', exact: true }).click();
    }
    await row.getByRole('button', { name: '查看详情', exact: true }).click();
    await page.getByRole('button', { name: '配置网络代理', exact: true }).click();
    await page.locator('.upstream-proxy-editor input').fill('socks5h://10.0.0.20:1080');
    assert.equal(await row.getByRole('button', { name: '收起详情', exact: true }).isEnabled(), false);
    assert.equal(await row.getByRole('button', { name: '编辑', exact: true }).isEnabled(), false);
    await page.getByRole('button', { name: '取消', exact: true }).click();
    await row.getByRole('button', { name: '收起详情', exact: true }).click();
    await page.setViewportSize({ width: 1440, height: 1000 });
    await row.getByRole('button', { name: '查看详情', exact: true }).click();
    await page.locator('.create-journey [data-workspace-toggle]').click();
    assert.equal(await page.locator('.provider-detail-workspace').count(), 0, 'creating uses the sole work area');
    assert.equal(await row.isVisible(), false, 'the directory is hidden while creating');
    await page.locator('.create-journey [data-workspace-toggle]').click();
    await page.getByRole('button', { name: '编辑', exact: true }).click();
    assert.equal(await row.isVisible(), false, 'another detail cannot replace an unsaved edit');
    const workspace = page.locator('.provider-edit-workspace');
    const name = workspace.getByLabel('上游名称', { exact: false });
    await name.fill('保留名称草稿');
    await page.getByRole('button', { name: '配置网络代理', exact: true }).click();
    const save = workspace.locator('.rjsf > button[type="submit"]');
    assert.equal(await save.isEnabled(), false);
    await page.locator('.upstream-proxy-editor input').fill('socks5h://10.0.0.10:1080');
    await page.getByRole('button', { name: '保存网络代理', exact: true }).click();
    await page.waitForFunction(() => !document.querySelector<HTMLButtonElement>('.provider-edit-workspace .rjsf > button[type="submit"]')?.disabled);
    assert.equal(await name.inputValue(), '保留名称草稿', 'proxy refresh preserves the independent name draft');
    assert.equal(await page.evaluate(() => window.formJourneyWrites), 1);
    await save.click();
    await page.locator('.provider-list').getByText('保留名称草稿', { exact: true }).waitFor();
    assert.equal(await page.evaluate(() => window.formJourneyWrites), 2, 'provider save succeeds against the new revision without retries');
    // Credential-generation change invalidates both the cached summary and
    // reset capability. Unknown discovery remains visible, but cannot prepare
    // a reset. These reads and the proxy update are in-memory only; no reset
    // endpoint (not even preparation) is allowed by this fixture.
    await page.goto(`${origin}/e2e/fixtures/form-journey.html?workflows&proxy-workflow&quota-generation`);
    await row.getByRole('button', { name: '查看详情', exact: true }).click();
    await page.getByRole('button', { name: '查看额度', exact: true }).click();
    await page.getByText('Codex 附加用量（代次 1 额度） · 供应商窗口', { exact: true }).waitFor();
    assert.match(await row.innerText(), /75/);
    await page.getByRole('button', { name: '额度重置选项', exact: true }).click();
    assert.equal(await page.getByRole('button', { name: '重置上游额度', exact: true }).isVisible(), true);
    await row.getByRole('button', { name: '收起详情', exact: true }).click();
    await row.getByRole('button', { name: '查看详情', exact: true }).click();
    await page.getByText('Codex 附加用量（代次 1 额度） · 供应商窗口', { exact: true }).waitFor();
    await page.evaluate(() => { window.deferNextFormQuotaRead = true; });
    await page.getByRole('button', { name: '刷新额度', exact: true }).click();
    await page.waitForFunction(() => window.formJourneyReads.filter(path => path.endsWith('/quota')).length === 2);
    await page.getByRole('button', { name: '配置网络代理', exact: true }).click();
    await page.locator('.upstream-proxy-editor input').fill('socks5h://10.0.0.30:1080');
    await page.getByRole('button', { name: '保存网络代理', exact: true }).click();
    await page.getByRole('button', { name: '查看额度', exact: true }).waitFor();
    assert.match(await row.innerText(), /尚未读取/);
    assert.equal(await page.getByText('Codex 附加用量（代次 1 额度） · 供应商窗口', { exact: true }).count(), 0);
    await page.getByRole('button', { name: '额度重置选项', exact: true }).click();
    assert.equal(
      await page.getByRole('button', { name: '重置上游额度', exact: true }).isEnabled(),
      false,
      'unknown capability must not allow reset preparation after a credential-generation change',
    );
    await page.getByRole('button', { name: '查看额度', exact: true }).click();
    await page.getByText('Codex 附加用量（代次 2 额度） · 供应商窗口', { exact: true }).waitFor();
    await page.evaluate(() => window.releaseFormQuotaRead());
    await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
    assert.match(await row.innerText(), /25/);
    assert.doesNotMatch(await row.innerText(), /75/);
    assert.equal(await page.getByText('Codex 附加用量（代次 1 额度） · 供应商窗口', { exact: true }).count(), 0);
    assert.equal(await page.getByRole('button', { name: '额度重置选项', exact: true }).count(), 0);
    await row.getByRole('button', { name: '收起详情', exact: true }).click();
    await row.getByRole('button', { name: '查看详情', exact: true }).click();
    await page.getByText('Codex 附加用量（代次 2 额度） · 供应商窗口', { exact: true }).waitFor();
    assert.equal(await page.evaluate(() => window.formJourneyWrites), 1, 'only the mocked proxy change was written; no reset operation was attempted');
    assert.deepEqual(errors, []);
  } finally { await browser.close(); await server.close(); }
});
