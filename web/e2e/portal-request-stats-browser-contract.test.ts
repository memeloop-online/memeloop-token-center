import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { mkdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

const upstreamId = '019f0000-0000-7000-8000-000000000052';
const routeId = '019f0000-0000-7000-8000-000000000031';
const bucket = (name: string, requests: number) => ({ name, requests, input_tokens: requests * 10, output_tokens: requests * 2, cost: '0', costs: [] });
const summary = (count: number) => ({ total_requests: count, successful_requests: count, failed_requests: 0, input_tokens: count * 10, output_tokens: count * 2, total_cost: '0', costs: [],
  cache_usage: { reported_read_tokens: count === 7 ? 0 : count * 4, reported_requests: count, known_read_tokens: count === 7 ? 0 : count * 4, known_input_tokens: count * 10, eligible_requests: count, unknown_requests: 0, hit_rate: count ? count === 7 ? 0 : 0.4 : null },
});

test('Portal request statistics use filtered API buckets, localized trends, and readable historical tooltips', { timeout: 60_000 }, async () => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    return test.skip('Chromium required');
  }
  const root = fileURLToPath(new URL('..', import.meta.url));
  const artifacts = `${root}/e2e-artifacts/ui-system/portal-request-stats`;
  await mkdir(artifacts, { recursive: true });
  const server = await createServer({ root, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 1440, height: 900 }, timezoneId: 'Asia/Shanghai' });
    await page.addInitScript(() => {
      if (!localStorage.getItem('mtc-locale')) localStorage.setItem('mtc-locale', 'en');
      localStorage.setItem('mtc.self.request-refresh-ms.v1', '0');
    });
    const reads: URL[] = [];
    let empty = false;
    let cacheState: 'known' | 'partial' | 'unknown' | 'zero' | 'noInput' = 'known';
    await page.route('**/self/v1/**', async route => {
      const url = new URL(route.request().url());
      if (url.pathname.endsWith('/key')) return route.fulfill({ json: {
        key_id: 'fixture-key', alias: 'Research workspace', currency: 'USD', credential_generation: 1, created_at: 1, available_balance: '100',
        policy: { enforcement_mode: 'prepaid', requests_per_minute: 60, tokens_per_minute: 6000, max_concurrency: 4, daily_budget: null, weekly_budget: null, lifetime_budget: null },
      } });
      const filtered = url.searchParams.get('model') === 'Historical model';
      if (url.pathname.endsWith('/stats')) {
        reads.push(url);
        const count = empty ? 0 : filtered ? 7 : 123456;
        const metrics = summary(count);
        if (cacheState === 'unknown') metrics.cache_usage = { reported_read_tokens: 0, reported_requests: 0, known_read_tokens: 0, known_input_tokens: 0, eligible_requests: 0, unknown_requests: count, hit_rate: null };
        if (cacheState === 'partial') metrics.cache_usage = { reported_read_tokens: (count - 1) * 4, reported_requests: count - 1, known_read_tokens: (count - 1) * 4, known_input_tokens: (count - 1) * 10, eligible_requests: count - 1, unknown_requests: 1, hit_rate: 0.4 };
        if (cacheState === 'zero' || cacheState === 'noInput') metrics.cache_usage = { reported_read_tokens: 0, reported_requests: count, known_read_tokens: 0, known_input_tokens: cacheState === 'zero' ? count * 10 : 0, eligible_requests: count, unknown_requests: 0, hit_rate: cacheState === 'zero' ? 0 : null };
        return route.fulfill({ json: { key_id: 'fixture-key', summary: metrics,
          by_day: empty ? [] : filtered ? [bucket('2026-10-09', 7)] : [bucket('2026-10-07', 100000), bucket('2026-10-08', 20000), bucket('2026-10-09', 3456)],
          by_model: empty ? [] : [bucket('Historical model', filtered ? 7 : 123456)], errors: [],
        } });
      }
      if (url.pathname.endsWith('/requests')) return route.fulfill({ json: empty ? [] : [{
        request_id: 'historical-request', created_at: Date.parse('2026-10-09T01:00:00Z'), completed_at: Date.parse('2026-10-09T01:00:01Z'),
        model: 'Historical model', protocol: 'openai', upstream_account_id: upstreamId, route_id: routeId,
        status_code: 200, duration_ms: 1000, input_tokens: 10, output_tokens: 2, cost: '0', currency: 'USD', error_code: null, archive_state: 'complete', usage_basis: 'provider_reported',
      }] });
      return route.fulfill({ json: [] });
    });
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/self-request-refresh.html`);
    await page.getByLabel('Client credential', { exact: true }).fill('fixture-credential');
    await page.getByRole('button', { name: 'Continue', exact: true }).click();
    await page.locator('.self-request-summary .analytics-metric-trend').first().waitFor();
    await page.screenshot({ path: `${artifacts}/initial-api-statistics.png`, fullPage: true });

    for (const locale of ['en', 'zh-CN'] as const) {
      await page.evaluate(value => localStorage.setItem('mtc-locale', value), locale);
      await page.reload();
      const total = page.getByRole('slider', { name: locale === 'en' ? 'Total requests' : '总请求', exact: true });
      await page.locator('.self-request-summary .analytics-metric-trend').first().waitFor();
      assert.equal(await page.locator('.self-request-summary .analytics-metric-trend').count(), 2);
      assert.equal(await page.locator('.self-request-summary .metric-exact').first().getAttribute('title'), '123,456', 'summary comes from the whole API scope, not the single loaded row');
      assert.match(await page.locator('.self-request-summary').innerText(), /40%/);
      await total.focus();
      await total.press('End');
      assert.match(await total.getAttribute('aria-valuetext') ?? '', /UTC.*3,456/);
      assert.equal(await total.getAttribute('aria-valuenow'), '3');
      await total.press('Home');
      assert.match(await total.getAttribute('aria-valuetext') ?? '', /UTC.*100,000/);
      await total.blur();
      await page.locator('.request-routing-info').focus();
      const tooltip = page.getByRole('tooltip');
      await tooltip.waitFor();
      const text = await tooltip.innerText();
      assert.match(text, locale === 'en' ? /Account name unavailable/ : /账号名称不可用/);
      assert.ok(!text.includes(upstreamId) && !text.includes(routeId), 'historical missing names never fall back to technical IDs');
      await page.locator('.request-routing-info').blur();
      for (const theme of ['dark', 'light'] as const) {
        await page.evaluate(value => { document.documentElement.dataset.theme = value; }, theme);
        for (const width of [390, 1440]) {
          await page.setViewportSize({ width, height: 900 });
          await page.screenshot({ path: `${artifacts}/stats-${locale}-${theme}-${width}.png`, fullPage: true });
          assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth), `${locale}/${theme}/${width} has no page overflow`);
        }
      }
    }
    for (const state of ['partial', 'unknown', 'zero', 'noInput'] as const) {
      cacheState = state;
      for (const locale of ['en', 'zh-CN'] as const) {
        await page.evaluate(value => localStorage.setItem('mtc-locale', value), locale);
        await page.reload();
        await page.locator('.self-request-summary .analytics-metric-trend').first().waitFor();
        const text = await page.locator('.self-request-summary').innerText();
        if (state === 'partial') assert.match(text, locale === 'en' ? /some requests have incomplete cache data/ : /部分请求未提供完整的缓存用量/);
        if (state === 'unknown') assert.match(text, locale === 'en' ? /Unknown/ : /未知/);
        if (state === 'zero') assert.match(text, /0%/);
        if (state === 'noInput') assert.match(text, locale === 'en' ? /no input tokens available/ : /没有可用于计算命中率的输入词元/);
        await page.screenshot({ path: `${artifacts}/cache-${state}-${locale}.png`, fullPage: true });
      }
    }
    cacheState = 'known';
    await page.evaluate(() => localStorage.setItem('mtc-locale', 'en'));
    await page.reload();
    await page.locator('.self-request-breakdown .bucket-heading').first().waitFor();
    const filteredStats = page.waitForResponse(response => new URL(response.url()).pathname.endsWith('/stats') && new URL(response.url()).searchParams.get('model') === 'Historical model');
    await page.locator('.self-request-breakdown button.bucket-heading').click();
    await filteredStats;
    await page.waitForFunction(() => document.querySelector('.self-request-summary .metric-exact')?.getAttribute('title') === '7');
    assert.equal(await page.getByLabel('Model', { exact: true }).inputValue(), 'Historical model');
    assert.equal(await page.locator('.self-request-summary .analytics-metric-trend').count(), 0, 'one API bucket cannot fabricate a curve');
    assert.equal(reads.at(-1)?.searchParams.get('model'), 'Historical model');
    assert.match(await page.locator('.self-request-summary').innerText(), /0%/);
    await page.screenshot({ path: `${artifacts}/filtered-single-day.png`, fullPage: true });
    empty = true;
    await page.getByRole('button', { name: 'Clear filters', exact: true }).click();
    await page.waitForFunction(() => document.querySelector('.self-request-summary .metric-exact')?.getAttribute('title') === '0');
    assert.equal(await page.locator('.self-request-summary [role="slider"]').count(), 0);
    assert.equal(await page.locator('.self-request-breakdown .empty').count(), 2);
    assert.equal(reads.at(-1)?.searchParams.has('model'), false);
    assert.match(await page.locator('.self-request-summary').innerText(), /Unknown/);
    await page.screenshot({ path: `${artifacts}/empty.png`, fullPage: true });
  } finally { await browser.close(); await server.close(); }
});
