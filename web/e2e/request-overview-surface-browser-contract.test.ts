import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';
import { formatMilliseconds, formatNumber, formatPercent } from '../src/format.js';
import { requestOverviewFacts } from './fixtures/request-overview-facts.js';

test('request popover stays non-modal across themes, locales and widths; models retain distinct account facts', { timeout: 60_000 }, async (t) => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required for request overview surface contracts');
    t.skip('Chromium is not installed');
    return;
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    for (const locale of ['en', 'zh-CN'] as const) {
      const page = await browser.newPage();
      await page.addInitScript((value) => localStorage.setItem('mtc-locale', value), locale);
      await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/request-overview-surface.html`);
      const trigger = page.getByRole('button', { name: locale === 'en' ? 'Filter' : '筛选', exact: true });
      await trigger.waitFor();
      assert.equal(await page.locator('[data-upstream-model="shared-model"]').count(), 1);
      assert.equal(await page.locator('[data-upstream-account-id]').count(), 4, 'all synthetic facts survive, including duplicate pairs');
      assert.equal(await page.locator('ol.monitoring-top-list, ol.monitoring-account-models').count(), 0, 'model grouping must not imply aggregate or globally ordered pair ranks');
      const scope = locale === 'en'
        ? 'Up to 10 account/model pairs ranked by terminal traffic in this window · Grouped by model, not an aggregate model ranking'
        : '窗口内按已完成流量排名的最多 10 个账号/模型组合 · 按模型分组展示，非模型聚合排名';
      assert.equal(await page.getByText(scope, { exact: true }).count(), 1);
      // These names are synthetic display labels, not real integration evidence.
      for (const name of ['Copilot', 'Cursor', 'Kimi']) assert.equal(await page.getByText(name, { exact: true }).count(), name === 'Copilot' ? 2 : 1);
      for (const [index, fact] of requestOverviewFacts.entries()) {
        const row = page.locator('[data-upstream-account-id]').filter({ has: page.getByText(`fixture-error-${index}`, { exact: true }) });
        assert.equal(await row.count(), 1);
        assert.equal(await row.getAttribute('data-upstream-account-id'), fact.upstream_account_id);
        const values = await row.locator('.monitoring-metric-list dd').allTextContents();
        assert.deepEqual(values.slice(0, 4), [formatNumber(fact.metrics.requests, locale),
          formatPercent(fact.metrics.successful_requests / fact.metrics.requests, locale),
          formatMilliseconds(fact.metrics.avg_duration_ms, locale), formatMilliseconds(fact.metrics.p95_duration_ms, locale)]);
        const cost = fact.metrics.costs[0];
        assert.equal(await row.locator('.monitoring-cost-lines > span').getAttribute('title'), `${cost.cost} ${cost.currency}`);
        const outcome = fact.terminal_outcomes[0];
        assert.equal(await row.locator('.monitoring-outcomes li').count(), 1);
        assert.equal(await row.locator('time').getAttribute('datetime'), new Date(outcome.created_at).toISOString());
        assert.match(await row.locator('.monitoring-outcomes').innerText(), new RegExp(`${outcome.duration_ms} ms`));
        assert.equal(await row.locator('.monitoring-outcomes .status.bad').count(), 1);
        assert.equal(await row.locator('.monitoring-outcomes').getByText(locale === 'en' ? 'Request' : '请求', { exact: true }).count(), 1);
      }
      for (const theme of ['dark', 'light']) {
        await page.evaluate((value) => { document.documentElement.dataset.theme = value; }, theme);
        for (const width of [320, 390, 768, 1024, 1440, 1920, 2560]) {
          await page.setViewportSize({ width, height: 900 });
          await trigger.click();
          const panel = page.locator('.typed-filter-dialog');
          await panel.waitFor();
          const surface = await panel.evaluate((element) => {
            const bounds = element.getBoundingClientRect();
            return { open: element.matches(':popover-open'), modal: element.hasAttribute('aria-modal'),
              left: bounds.left, right: bounds.right, background: getComputedStyle(element).backgroundColor,
              backdrop: getComputedStyle(element, '::backdrop').backgroundColor,
              overflow: document.documentElement.scrollWidth > innerWidth };
          });
          assert.equal(surface.open, true);
          assert.equal(surface.modal, false);
          assert.equal(surface.backdrop, 'rgba(0, 0, 0, 0)');
          assert.equal(surface.background, theme === 'light' ? 'rgb(255, 255, 255)' : 'rgb(13, 28, 32)');
          assert.ok(surface.left >= 0 && surface.right <= width, JSON.stringify({ locale, theme, width, surface }));
          assert.equal(surface.overflow, false);
          await page.keyboard.press('Escape');
          await panel.waitFor({ state: 'detached' });
          assert.equal(await panel.count(), 0);
          assert.equal(await trigger.evaluate((element) => document.activeElement === element), true);
          await trigger.click();
          // Native popovers must not trap focus or make the rest of the page inert.
          const outside = page.locator('#outside-control');
          await outside.focus();
          assert.equal(await outside.evaluate((element) => document.activeElement === element), true);
          // Click an uncovered outside point; light dismissal must synchronize React state.
          await page.mouse.click(width - 2, 2);
          await panel.waitFor({ state: 'detached' });
          assert.equal(await trigger.getAttribute('aria-expanded'), 'false');
        }
      }
      await page.close();
    }
  } finally {
    await browser.close();
    await server.close();
  }
});
