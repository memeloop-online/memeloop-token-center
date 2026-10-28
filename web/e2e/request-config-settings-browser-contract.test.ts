import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { mkdir, readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';
import { translationCatalogs } from '../src/i18n.js';
import { parseConfigInput, requestConfigFields, requestConfigPurposes } from '../src/operator/requestConfig.js';

test('request-config metadata has localized names and tooltips, never bare field keys', async () => {
  for (const locale of ['zh-CN', 'en'] as const) {
    const catalog = translationCatalogs[locale];
    for (const field of requestConfigFields) {
      const label = catalog[`settings.requestConfigField.${field.key}`];
      const hint = catalog[`settings.requestConfigField.${field.key}Hint`];
      assert.ok(label && hint, `${locale} missing copy for ${field.key}`);
      assert.ok(!label.includes(field.key) && !hint.includes('_'), `${locale} copy must not expose raw keys`);
    }
    for (const purpose of requestConfigPurposes) {
      const label = catalog[`settings.requestConfigPurpose.${purpose}`];
      assert.ok(label && label !== purpose, `${locale} missing purpose label for ${purpose}`);
    }
    for (const key of ['Title', 'Description', 'Defaults', 'PurposeOverrides', 'SourceDefault', 'SourceOverride', 'Effective', 'Saved', 'Cleared', 'LoadFailed', 'Unavailable', 'Unlimited', 'ClearOverride']) {
      assert.ok(catalog[`settings.requestConfig${key}`], `${locale} missing settings.requestConfig${key}`);
    }
  }
  const page = await readFile(new URL('../src/operator/pages/SystemSettingsPage.tsx', import.meta.url), 'utf8');
  assert.match(page, /<RequestConfigSettings/);
  assert.doesNotMatch(page, /\/internal\/v1\/request-config/, 'the page must not bypass the typed wiring module');
  const api = await readFile(new URL('../src/operator/requestConfigApi.ts', import.meta.url), 'utf8');
  assert.match(api, /request-config\/settings/);
  assert.match(api, /contract_version/);
});
test('request-config input parsing honors units, ranges and explicit no-limit values', () => {
  const [timeout, idle, body] = requestConfigFields;
  assert.deepEqual(parseConfigInput('45', timeout), { ok: true, value: 45_000 });
  assert.deepEqual(parseConfigInput('', timeout), { ok: false }, 'the overall timeout stays required');
  assert.deepEqual(parseConfigInput('', idle), { ok: true, value: null }, 'blank nullable values mean no extra limit');
  assert.deepEqual(parseConfigInput('', body), { ok: true, value: null });
  assert.deepEqual(parseConfigInput('16', body), { ok: true, value: 16 * 1_048_576 });
  assert.equal(parseConfigInput('0.5', timeout).ok, false, 'sub-second timeouts are rejected');
  assert.equal(parseConfigInput('9999', timeout).ok, false);
  assert.equal(parseConfigInput('abc', body).ok, false);
});
test('request-config settings inherit, override, save and clear across locale and viewport', { timeout: 120_000 }, async (context) => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required for the request-config settings contract');
    context.skip('Chromium is not installed'); return;
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const artifacts = fileURLToPath(new URL('../e2e-artifacts/ui-system/request-config-settings/', import.meta.url));
  await mkdir(artifacts, { recursive: true });
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
    page.setDefaultTimeout(8_000);
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/request-config-settings.html`);
    const card = page.locator('.request-config');
    await card.getByRole('heading', { name: '请求配置', exact: true }).waitFor();

    context.diagnostic('loaded state shows global defaults and inheritance');
    assert.equal(await card.locator('#request-config-default-timeout_ms').inputValue(), '30');
    assert.equal(await card.locator('#request-config-default-max_request_bytes').inputValue(), '');
    await card.getByText('实际生效：不额外限制').first().waitFor();
    await card.getByRole('button', { name: '文本对话', exact: true }).click();
    const textPurpose = card.locator('.request-config-purpose').first();
    await textPurpose.locator('#request-config-text-timeout_ms').waitFor();
    assert.equal(await textPurpose.locator('#request-config-text-timeout_ms').inputValue(), '60');
    assert.match(await textPurpose.locator('#request-config-text-stream_idle_timeout_ms').getAttribute('placeholder') ?? '', /继承：15 秒/, 'an unset override inherits the global default');
    await textPurpose.getByText('已覆盖').first().waitFor();
    await textPurpose.getByText('实际生效：60 秒').waitFor();
    await textPurpose.getByText('继承默认').first().waitFor();
    await page.screenshot({ path: `${artifacts}/loaded-zh-1440.png`, fullPage: true });

    context.diagnostic('saving an override updates the effective value dynamically');
    await textPurpose.locator('#request-config-text-stream_idle_timeout_ms').fill('20');
    await textPurpose.getByRole('button', { name: '保存覆盖', exact: true }).click();
    await card.getByRole('status').filter({ hasText: '已保存' }).waitFor();
    await textPurpose.getByText('实际生效：20 秒').waitFor();
    assert.equal(await textPurpose.locator('#request-config-text-stream_idle_timeout_ms').inputValue(), '20');
    await page.screenshot({ path: `${artifacts}/override-saved-zh-1440.png`, fullPage: true });

    context.diagnostic('clearing restores inheritance without a reload');
    await textPurpose.getByRole('button', { name: '清除覆盖', exact: true }).click();
    await card.getByRole('status').filter({ hasText: '已清除' }).waitFor();
    await textPurpose.getByText('实际生效：30 秒').waitFor();
    assert.equal(await textPurpose.getByText('已覆盖').count(), 0);
    assert.equal(await textPurpose.locator('#request-config-text-timeout_ms').inputValue(), '');

    context.diagnostic('a slow save keeps the current form feedback while requests continue');
    await page.evaluate(() => { (window as unknown as { requestConfigFixture: { delaySave: boolean } }).requestConfigFixture.delaySave = true; });
    await textPurpose.locator('#request-config-text-timeout_ms').fill('45');
    await textPurpose.getByRole('button', { name: '保存覆盖', exact: true }).click();
    await textPurpose.getByRole('button', { name: /载入中/ }).waitFor();
    assert.equal(await textPurpose.locator('#request-config-text-timeout_ms').inputValue(), '45', 'the in-flight draft is preserved while the save runs');
    await textPurpose.getByText('实际生效：30 秒').waitFor();
    await page.screenshot({ path: `${artifacts}/saving-pending-zh-1440.png`, fullPage: true });
    await page.evaluate(() => { const fixture = (window as unknown as { requestConfigFixture: { delaySave: boolean; releaseSave: () => void } }).requestConfigFixture; fixture.delaySave = false; fixture.releaseSave(); });
    await card.getByRole('status').filter({ hasText: '已保存' }).waitFor();
    await textPurpose.getByText('实际生效：45 秒').waitFor();

    context.diagnostic('validation blocks impossible values before saving');
    await textPurpose.locator('#request-config-text-timeout_ms').fill('0.1');
    assert.equal(await textPurpose.getByRole('button', { name: '保存覆盖', exact: true }).isDisabled(), true);
    await textPurpose.getByText(/请输入有效数值/).waitFor();
    await textPurpose.locator('#request-config-text-timeout_ms').fill('45');

    context.diagnostic('english wide layout');
    await page.getByRole('button', { name: 'Switch language' }).click();
    await card.getByRole('heading', { name: 'Request configuration', exact: true }).waitFor();
    const textPurposeEn = card.locator('.request-config-purpose').first();
    if (!(await textPurposeEn.locator('#request-config-text-timeout_ms').isVisible())) await card.getByRole('button', { name: 'Text chat', exact: true }).click();
    await textPurposeEn.getByText('Effective: 45 s').waitFor();
    await textPurposeEn.getByText('Overridden').first().waitFor();
    await card.getByText('Effective: No extra limit').first().waitFor();
    await page.screenshot({ path: `${artifacts}/override-en-1440.png`, fullPage: true });

    context.diagnostic('narrow layouts contain content without horizontal overflow');
    await page.setViewportSize({ width: 390, height: 844 });
    await page.getByRole('button', { name: 'Switch language' }).click();
    await card.getByRole('heading', { name: '请求配置', exact: true }).waitFor();
    const overflow = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
    assert.ok(overflow <= 0, `narrow layout must not overflow horizontally: ${overflow}px`);
    await page.screenshot({ path: `${artifacts}/loaded-zh-390.png`, fullPage: true });
    await page.getByRole('button', { name: 'Switch language' }).click();
    await card.getByRole('heading', { name: 'Request configuration', exact: true }).waitFor();
    await page.screenshot({ path: `${artifacts}/loaded-en-390.png`, fullPage: true });

    context.diagnostic('a deployment without the endpoint keeps the rest of the page usable');
    await page.evaluate(() => { (window as unknown as { requestConfigFixture: { unavailable: boolean } }).requestConfigFixture.unavailable = true; });
    await page.reload();
    await card.getByText('This deployment does not offer request configuration yet').waitFor();
    await page.getByText('No filter-assistant model has been selected.').waitFor();
  } finally {
    await browser.close();
    await server.close();
  }
});
