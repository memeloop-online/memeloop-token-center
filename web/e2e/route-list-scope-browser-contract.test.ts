import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import { mkdir } from 'node:fs/promises';
import test from 'node:test';
import { chromium, type Locator } from 'playwright';
import { createIsolatedFixtureServer as createServer } from './support/isolated-vite-server.js';

declare global { interface Window { routeListWrites: number } }

test('route list exposes group-only candidate scope and readable models without changing routing', { timeout: 45_000 }, async () => {
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen(); const address = server.httpServer!.address(); assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  const artifacts = fileURLToPath(new URL('../e2e-artifacts/route-list-scope/', import.meta.url));
  await mkdir(artifacts, { recursive: true });
  try {
    const page = await browser.newPage({ hasTouch: true }); const errors: string[] = [];
    page.on('pageerror', error => errors.push(error.message));
    const describedTooltip = async (trigger: Locator) => {
      const element = await trigger.elementHandle(); assert.ok(element);
      const description = await page.waitForFunction(element =>
        (element.getAttribute('aria-describedby') ?? '').split(/\s+/)
          .find(id => document.getElementById(id)?.getAttribute('role') === 'tooltip') || false,
      element);
      const id = await description.jsonValue(); assert.equal(typeof id, 'string');
      const tooltip = page.locator(`[role="tooltip"][id=${JSON.stringify(id)}]`);
      await tooltip.waitFor();
      return tooltip;
    };
    for (const locale of ['zh-CN', 'en']) {
      await page.addInitScript(value => localStorage.setItem('mtc-locale', value), locale);
      await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/route-list-scope.html`);
      const rows = page.locator('.model-route-list tbody tr');
      const group = rows.filter({ hasText: 'Kimi group-only' });
      await group.getByText('Kimi 模型组', { exact: true }).waitFor();
      const range = group.getByRole('button', { name: locale === 'en' ? '2 candidate accounts' : '2 个候选账号', exact: true });
      assert.equal(await rows.filter({ hasText: 'Kimi mixed' }).getByRole('button', { name: locale === 'en' ? '1 candidate accounts' : '1 个候选账号', exact: true }).count(), 1);
      assert.equal(await rows.filter({ hasText: 'Kimi empty' }).getByText(locale === 'en' ? '0 candidate accounts' : '0 个候选账号', { exact: true }).count(), 1);
      assert.equal(await rows.filter({ hasText: 'Kimi unknown' }).getByText(locale === 'en' ? 'Account range unknown' : '账号范围未知', { exact: true }).count(), 1);
      assert.match(await rows.filter({ hasText: 'Kimi direct' }).locator('.route-list-source-name').innerText(), /Kimi personal account/);
      const mixedRange = rows.filter({ hasText: 'Kimi mixed' }).getByRole('button', { name: locale === 'en' ? '1 candidate accounts' : '1 个候选账号', exact: true });
      await mixedRange.click();
      const mixedTip = await describedTooltip(mixedRange);
      assert.match(await mixedTip.innerText(), /团队排除组/);
      assert.doesNotMatch(await mixedTip.innerText(), /Kimi team account/, 'the list must not re-expand excluded members beyond the server candidate set');
      await page.keyboard.press('Escape');
      await mixedTip.waitFor({ state: 'hidden' });
      for (const [width, theme] of [[390, 'light'], [1440, 'dark']] as const) {
        await page.setViewportSize({ width, height: 900 }); await page.evaluate(theme => { document.documentElement.dataset.theme = theme; }, theme);
        await range.focus(); await range.press('Shift+Tab'); await page.keyboard.press('Tab');
        assert.equal(await range.evaluate(el => el === document.activeElement), true);
        const tip = await describedTooltip(range);
        assert.match(await tip.innerText(), /Kimi personal account/); assert.match(await tip.innerText(), /Kimi team account/);
        await page.keyboard.press('Escape'); await tip.waitFor({ state: 'hidden' });
        await range.tap(); await describedTooltip(range);
        const bounds = await tip.boundingBox(); assert.ok(bounds && bounds.x >= -1 && bounds.x + bounds.width <= width + 1);
        const model = group.locator('.route-model-name').first();
        const style = await model.evaluate(el => { const cs = getComputedStyle(el); const probe = document.createElement('span'); probe.style.color = 'var(--colorNeutralForeground1)'; el.append(probe); const expected = getComputedStyle(probe).color; probe.remove(); return { color: cs.color, expected, weight: Number(cs.fontWeight), text: el.textContent }; });
        assert.equal(style.color, style.expected); assert.ok(style.weight >= 600); assert.equal(style.text, 'Kimi group-only');
        assert.equal(await group.locator('.route-model-name').last().textContent(), 'kimi-k2.5');
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false);
        await page.screenshot({ path: `${artifacts}/route-list-${locale}-${width}-${theme}.png`, fullPage: true });
        await page.keyboard.press('Escape');
        await tip.waitFor({ state: 'hidden' });
      }
      assert.equal(await page.evaluate(() => window.routeListWrites), 0);
    }
    assert.deepEqual(errors, []);
  } finally { await browser.close(); await server.close(); }
});
