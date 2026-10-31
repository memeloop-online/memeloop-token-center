import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { mkdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';
import { translationCatalogs } from '../src/i18n.js';
import { parseConfigInput, requestConfigFields, requestConfigPurposes, type HttpSettings } from '../src/operator/requestConfig.js';

test('HTTP fields and purposes have Chinese and English names, explanations and units', () => {
  for (const locale of ['zh-CN', 'en'] as const) {
    const catalog = translationCatalogs[locale];
    for (const field of requestConfigFields) {
      assert.ok(catalog[`settings.requestConfigField.${field.key}`]);
      assert.ok(catalog[`settings.requestConfigField.${field.key}Hint`]);
    }
    for (const purpose of requestConfigPurposes) assert.ok(catalog[`settings.requestConfigPurpose.${purpose}`]);
    for (const key of ['Revision', 'SourceDefault', 'SourceBuiltin', 'SourceOverride', 'Disable', 'Conflict', 'Permission']) assert.ok(catalog[`settings.requestConfig${key}`]);
  }
});

test('HTTP inputs distinguish inheritance, disabled deadlines and JS-safe integer values', () => {
  for (const field of requestConfigFields) {
    assert.deepEqual(parseConfigInput('', field), { ok: true, value: undefined });
    assert.deepEqual(parseConfigInput('9007199254740991', field), { ok: true, value: Number.MAX_SAFE_INTEGER });
    for (const invalid of ['-1', '1.5', '1e3', '0x10', '9007199254740992']) assert.equal(parseConfigInput(invalid, field).ok, false);
    assert.deepEqual(parseConfigInput('off', field), field.nullable ? { ok: true, value: null } : { ok: false });
    assert.deepEqual(parseConfigInput('0', field), field.nullable ? { ok: false } : { ok: true, value: 0 });
    assert.deepEqual(parseConfigInput('1', field), { ok: true, value: 1 });
  }
});

type FixtureControl = { failLoad: boolean; unavailable: boolean; forbidden: boolean; delaySave: boolean; failSave: boolean; conflict: boolean; releaseSave: () => void; lastBody: { expected_revision: number; settings: HttpSettings } | null };
type FixtureWindow = Window & typeof globalThis & { requestConfigFixture: FixtureControl };

test('HTTP settings save full documents, preserve inheritance and drafts, and expose active revisions', { timeout: 120_000 }, async (context) => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required');
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
    const page = await browser.newPage({ viewport: { width: 1440, height: 1000 } });
    page.setDefaultTimeout(8_000);
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/request-config-settings.html`);
    const card = page.locator('.request-config');
    await card.getByText('当前实例生效版本：7').waitFor();
    await card.getByRole('button', { name: 'Codex 请求', exact: true }).click();
    const purpose = card.locator('.request-config-purpose').filter({ has: page.locator('#request-config-codex-read_timeout_ms') });
    const read = purpose.locator('#request-config-codex-read_timeout_ms');
    assert.equal(await read.inputValue(), '');
    assert.match(await read.getAttribute('placeholder') ?? '', /继承：15000 毫秒/);
    await purpose.getByText('用途覆盖', { exact: true }).waitFor();
    await purpose.getByText('内置默认', { exact: true }).first().waitFor();
    await read.fill('45000');
    await page.getByRole('button', { name: 'Switch tenant' }).click();
    assert.equal(await read.inputValue(), '45000');
    await page.getByRole('button', { name: 'Switch tenant' }).click();
    await read.fill('');
    await page.screenshot({ path: `${artifacts}/loaded-zh-1440.png`, fullPage: true });

    await purpose.locator('.request-config-row').filter({ has: page.locator('#request-config-codex-read_timeout_ms') }).getByRole('checkbox', { name: '不设限' }).check();
    await purpose.getByRole('button', { name: '保存覆盖', exact: true }).click();
    await card.getByText('当前实例生效版本：8').waitFor();
    const body = await page.evaluate(() => (window as FixtureWindow).requestConfigFixture.lastBody);
    assert.equal(body?.expected_revision, 7);
    assert.equal(body?.settings.purposes.codex?.read_timeout_ms, null);
    assert.equal(body?.settings.purposes.oauth?.request_timeout_ms, 90000);
    assert.equal(body?.settings.defaults.read_timeout_ms, 15000);
    assert.deepEqual((body?.settings.defaults as Record<string, unknown>).future_default, { retain: true });
    assert.equal((body?.settings.purposes.codex as Record<string, unknown>).future_purpose_field, 'retain');
    assert.equal((body?.settings.purposes as Record<string, { request_timeout_ms?: number }>).future_purpose.request_timeout_ms, 1200);
    await page.screenshot({ path: `${artifacts}/disabled-override-zh-1440.png`, fullPage: true });

    await purpose.getByRole('button', { name: '清除覆盖', exact: true }).click();
    await card.getByText('当前实例生效版本：9').waitFor();
    assert.equal(await page.evaluate(() => 'codex' in ((window as FixtureWindow).requestConfigFixture.lastBody?.settings.purposes ?? {})), false);
    assert.equal(await read.inputValue(), '');
    await purpose.getByText('配置解析：15000 毫秒').waitFor();

    await read.fill('1');
    await page.evaluate(() => { (window as FixtureWindow).requestConfigFixture.delaySave = true; });
    await purpose.getByRole('button', { name: '保存覆盖', exact: true }).click();
    assert.equal(await read.inputValue(), '1');
    await purpose.getByText('配置解析：15000 毫秒').waitFor();
    await page.screenshot({ path: `${artifacts}/saving-zh-1440.png`, fullPage: true });
    await page.evaluate(() => { const fixture = (window as FixtureWindow).requestConfigFixture; fixture.delaySave = false; fixture.releaseSave(); });
    await card.getByText('当前实例生效版本：10').waitFor();
    await purpose.getByText('配置解析：1 毫秒').waitFor();

    const defaults = card.locator('.request-config-scope').first();
    await defaults.locator('#request-config-default-read_timeout_ms').fill('25000');
    await defaults.locator('#request-config-default-pool_max_idle_per_host').fill('0');
    const defaultRequest = defaults.locator('.request-config-row').filter({ has: page.locator('#request-config-default-request_timeout_ms') });
    await defaultRequest.getByRole('checkbox', { name: '不设限' }).check();
    await defaults.getByRole('button', { name: '保存默认' }).click();
    await card.getByText('当前实例生效版本：11').waitFor();
    const defaultBody = await page.evaluate(() => (window as FixtureWindow).requestConfigFixture.lastBody);
    assert.equal(defaultBody?.settings.defaults.request_timeout_ms, null);
    assert.deepEqual((defaultBody?.settings.defaults as Record<string, unknown>).future_default, { retain: true });
    await defaultRequest.getByText('全局默认', { exact: true }).waitFor();
    await defaultRequest.getByText('配置解析：不额外限制', { exact: true }).waitFor();
    await purpose.getByText('配置解析：1 毫秒').waitFor();
    await purpose.getByText('配置解析：0 条').waitFor();
    await purpose.getByRole('button', { name: '清除覆盖', exact: true }).click();
    await card.getByText('当前实例生效版本：12').waitFor();
    await purpose.getByText('配置解析：25000 毫秒').waitFor();

    await read.fill('0');
    assert.equal(await purpose.getByRole('button', { name: '保存覆盖', exact: true }).isDisabled(), true);
    await read.fill('45000');
    await page.evaluate(() => { (window as FixtureWindow).requestConfigFixture.failSave = true; });
    await purpose.getByRole('button', { name: '保存覆盖', exact: true }).click();
    await card.getByRole('alert').filter({ hasText: 'Save rejected' }).waitFor();
    assert.equal(await read.inputValue(), '45000');
    await page.evaluate(() => { const fixture = (window as FixtureWindow).requestConfigFixture; fixture.failSave = false; fixture.conflict = true; });
    await purpose.getByRole('button', { name: '保存覆盖', exact: true }).click();
    await card.getByText(/设置已被其他操作更新/).waitFor();
    assert.equal(await read.inputValue(), '45000');
    await page.screenshot({ path: `${artifacts}/conflict-zh-1440.png`, fullPage: true });
    await card.getByRole('button', { name: '重新载入并替换当前输入' }).click();
    await card.getByText('当前实例生效版本：12').waitFor();
    assert.equal(await read.inputValue(), '');

    for (const width of [1440, 390]) {
      await page.setViewportSize({ width, height: 900 });
      for (const locale of ['en', 'zh']) {
        await page.getByRole('button', { name: 'Switch language' }).click();
        await card.getByRole('heading', { name: locale === 'en' ? 'HTTP request settings' : 'HTTP 请求设置', exact: true }).waitFor();
        assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth));
        await page.screenshot({ path: `${artifacts}/loaded-${locale}-${width}.png`, fullPage: true });
      }
    }
    await page.evaluate(() => { (window as FixtureWindow).requestConfigFixture.forbidden = true; });
    await page.getByRole('button', { name: 'Reload settings' }).click();
    await card.getByText(/需要具有对应读取或写入权限的全局服务凭据/).waitFor();
    await page.screenshot({ path: `${artifacts}/permission-zh-390.png`, fullPage: true });
    await page.evaluate(() => { const fixture = (window as FixtureWindow).requestConfigFixture; fixture.forbidden = false; fixture.failLoad = true; });
    await card.getByRole('button', { name: '重试', exact: true }).click();
    await card.getByText('Store unavailable').waitFor();
    await page.evaluate(() => { const fixture = (window as FixtureWindow).requestConfigFixture; fixture.failLoad = false; fixture.unavailable = true; });
    await card.getByRole('button', { name: '重试', exact: true }).click();
    await card.getByText('此部署尚未提供请求配置，现有请求行为不变。').waitFor();
    await page.getByText('尚未选择筛选助手模型。').waitFor();
  } finally {
    await browser.close();
    await server.close();
  }
});
