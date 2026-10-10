import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer } from './support/isolated-vite-server.js';
import { editProviderAccount } from './support/provider-account-navigation.js';

test('upstream account notes edit, preview, save, failure, conflict, clear, cancel and scope isolation', { timeout: 90_000 }, async () => {
  const root = fileURLToPath(new URL('..', import.meta.url));
  const server = await createIsolatedFixtureServer({ root, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
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
    const artifacts = `${root}/e2e-artifacts/ui-system/upstream-notes`; await mkdir(artifacts, { recursive: true });
    await page.goto(`${origin}/e2e/fixtures/form-journey.html?workflows=1&proxy-workflow=1&notes-workflow=1`);
    await editProviderAccount(page);
    const workspace = page.locator('.provider-edit-workspace');
    const notes = workspace.locator('.upstream-notes-editor');
    const area = notes.getByLabel('备注', { exact: true });
    const save = notes.getByRole('button', { name: '保存备注', exact: true });
    const cancel = notes.getByRole('button', { name: '取消', exact: true });
    await area.waitFor();
    assert.equal(await save.isDisabled(), true, 'nothing to save before editing');

    // Toolbar inserts a link template and typing replaces the selected URL.
    await area.click();
    await notes.getByRole('button', { name: '链接', exact: true }).click();
    await page.waitForFunction(() => document.activeElement instanceof HTMLTextAreaElement && document.activeElement.value === '[链接文字](https://)');
    await page.keyboard.type('https://example.com/docs');
    assert.equal(await area.inputValue(), '[链接文字](https://example.com/docs)');
    await cancel.click();
    assert.equal(await area.inputValue(), '', 'cancel restores the saved baseline');

    // Successful save: explicit notes, tenant identity and the revision fence.
    await area.fill('官网 [文档](https://example.com/docs)');
    await save.click();
    await notes.locator('.notice.success').getByText('备注已保存。', { exact: true }).waitFor();
    assert.deepEqual(await page.evaluate(() => window.formJourneyLastNotesWrite), {
      tenant_external_id: 'fixture', notes: '官网 [文档](https://example.com/docs)', expected_updated_at: 1,
    });
    assert.equal(await save.isDisabled(), true, 'saved content is no longer dirty');

    // Preview renders the https link safely and never embeds images.
    await notes.getByRole('button', { name: '预览', exact: true }).click();
    const link = notes.locator('.upstream-notes-preview').getByRole('link', { name: '文档' });
    await link.waitFor();
    assert.equal(await link.getAttribute('href'), 'https://example.com/docs');
    assert.equal(await link.getAttribute('rel'), 'noopener noreferrer');
    assert.equal(await link.getAttribute('target'), '_blank');
    await page.screenshot({ path: `${artifacts}/notes-saved-preview-1440.png`, fullPage: true });
    await notes.getByRole('button', { name: '编辑', exact: true }).click();

    // Unsafe links never become clickable in the preview.
    await area.fill('[点我](javascript:alert(1)) 与 [凭证](https://user:password@example.com/x)');
    await notes.getByRole('button', { name: '预览', exact: true }).click();
    await notes.locator('.upstream-notes-preview').waitFor();
    assert.equal(await notes.locator('.upstream-notes-preview a').count(), 0);
    await notes.getByRole('button', { name: '编辑', exact: true }).click();
    await cancel.click();
    assert.equal(await area.inputValue(), '官网 [文档](https://example.com/docs)');

    // A failed save keeps the draft and explains the cause.
    await area.fill('官网 [文档](https://example.com/docs)\n新增一行');
    await page.evaluate(() => { window.failNextFormNotesWrite = true; });
    await save.click();
    await notes.getByRole('alert').getByText('模拟备注保存失败，草稿仍在', { exact: false }).waitFor();
    assert.equal(await area.inputValue(), '官网 [文档](https://example.com/docs)\n新增一行', 'failure keeps the draft');
    assert.equal(await save.isDisabled(), false, 'draft can be saved again');

    // A revision conflict explains the next step and keeps the draft.
    await page.evaluate(() => { window.conflictNextFormNotesWrite = true; });
    await save.click();
    await notes.getByRole('alert').getByText('该账号刚被其他操作更新。请关闭后重新打开编辑，再修改备注。', { exact: true }).waitFor();
    assert.equal(await area.inputValue(), '官网 [文档](https://example.com/docs)\n新增一行', 'conflict keeps the draft');
    await cancel.click();

    // Clearing saves an explicit null and advances the fence from the notes response.
    await area.fill('   ');
    await save.click();
    await notes.locator('.notice.success').getByText('备注已保存。', { exact: true }).waitFor();
    assert.deepEqual(await page.evaluate(() => window.formJourneyLastNotesWrite), {
      tenant_external_id: 'fixture', notes: null, expected_updated_at: 2,
    });
    await notes.getByRole('button', { name: '预览', exact: true }).click();
    await notes.getByText('暂无备注。', { exact: true }).waitFor();
    await notes.getByRole('button', { name: '编辑', exact: true }).click();

    // A late response must not publish into a scope that no longer owns the workspace.
    await area.fill('晚到的响应不应写入新范围');
    await page.evaluate(() => { window.deferNextFormNotesWrite = true; });
    await save.click();
    await notes.getByRole('button', { name: '正在保存…', exact: true }).waitFor();
    await page.evaluate(() => window.changeFormProviderScope('tenant'));
    await editProviderAccount(page);
    const nextNotes = page.locator('.provider-edit-workspace .upstream-notes-editor');
    const nextArea = nextNotes.getByLabel('备注', { exact: true });
    await nextArea.waitFor();
    assert.equal(await nextArea.inputValue(), '', 'the new scope only shows its own read');
    await page.evaluate(() => window.releaseFormNotesWrite());
    await page.evaluate(() => new Promise(resolve => setTimeout(resolve, 0)));
    await page.evaluate(() => new Promise(resolve => setTimeout(resolve, 0)));
    assert.equal(await nextNotes.locator('.notice.success').count(), 0, 'late response shows no success in the new scope');
    assert.equal(await nextArea.inputValue(), '', 'late response never overwrites the new scope editor');

    // The deferred write did reach the fixture; a fresh scope reads it back.
    await page.evaluate(() => window.changeFormProviderScope('tenant'));
    await editProviderAccount(page);
    const freshArea = page.locator('.provider-edit-workspace .upstream-notes-editor').getByLabel('备注', { exact: true });
    await freshArea.waitFor();
    assert.equal(await freshArea.inputValue(), '晚到的响应不应写入新范围');
    assert.deepEqual(errors, [], 'no page errors across the whole journey');
  } finally {
    await browser.close();
    await server.close();
  }
});
