import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer as createServer } from './support/isolated-vite-server.js';
import { fixtureAssets } from './support/fixture-assets.js';

// Render the production index/main/Application/AppShell/Operator graph, not a
// hand-assembled page fixture. Only network data is synthetic and core-owned.
test('production AppShell typography and actions share Fluent tokens in both themes and sizes', { timeout: 90_000 }, async () => {
  const root = fileURLToPath(new URL('..', import.meta.url));
  const server = await createServer({ root, configFile: false, plugins: [fixtureAssets()], logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const origin = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  const artifacts = `${root}/e2e-artifacts/ui-system/app-shell`;
  await mkdir(artifacts, { recursive: true });
  try {
    const page = await browser.newPage();
    const unexpected: string[] = [], errors: string[] = [];
    page.on('pageerror', error => errors.push(error.message));
    const start = Date.UTC(2026, 8, 15, 12);
    const base = { request_id: 'synthetic-running', created_at: start, completed_at: null, model: 'sample-model', protocol: 'openai', status_code: null, duration_ms: null, input_tokens: 120, output_tokens: 30, cost: '0', error_code: null };
    const requests = [base,
      { ...base, request_id: 'synthetic-success', created_at: start + 30_000, completed_at: start + 50_000, status_code: 200, duration_ms: 20_000, input_tokens: 200, output_tokens: 100, usage_basis: 'provider_reported' },
      { ...base, request_id: 'synthetic-failure', created_at: start + 60_000, completed_at: start + 90_000, status_code: 502, duration_ms: 30_000, input_tokens: 40, output_tokens: 10, usage_basis: 'provider_reported' },
    ];
    const metrics = { requests: 3, success: 1, failed: 1, input_tokens: 0, output_tokens: 0, cached_input_tokens: 0, cache_write_tokens: 0, generation_units: 0, avg_duration_ms: 25_000, p95_duration_ms: 30_000, costs: [] };
    await page.route('**/*', async route => {
      const request = route.request(), url = new URL(request.url());
      if (url.origin !== origin) { unexpected.push(url.origin); return route.abort(); }
      if (!url.pathname.startsWith('/internal/')) return route.continue();
      const json = (data: unknown) => route.fulfill({ contentType: 'application/json', body: JSON.stringify(data) });
      if (url.pathname === '/internal/v1/requests/query' && request.method() === 'POST') return json({ requests, next_cursor: 'synthetic-older' });
      if (request.method() !== 'GET') { unexpected.push(`${request.method()} ${url.pathname}`); return route.abort(); }
      if (url.pathname === '/internal/v1/tenants') return json([{ id: 'synthetic', external_id: 'default', name: 'Design acceptance', created_at: 1, updated_at: 1 }]);
      if (url.pathname === '/internal/v1/requests') return json(requests);
      if (url.pathname === '/internal/v1/monitoring-snapshot') return json({ contract_version: 'v1', generated_at: start, scope: 'tenant', tenant_external_id: 'default', from_created_at: start - 3_600_000, to_created_at: start, granularity: 'hour', latency_is_approximate: true, latency_method: 'fixed_histogram_upper_bound_capped_60000ms', summary: { requests: 3, successful_requests: 1, failed_requests: 1, avg_duration_ms: 25_000, p95_duration_ms: 30_000, costs: [] }, freshness: { latest_terminal_created_at: start, age_millis: 0 }, health: { version: 'upstream_breaker_v1', status: 'unknown', observed_at: start }, top_upstream_models: [] });
      if (url.pathname === '/internal/v1/usage-analysis/trends') return json({ from_created_at: start - 3_600_000, to_created_at: start, granularity: 'hour', time_zone: 'UTC', p95_is_approximate: true, p95_method: 'fixed_histogram_upper_bound_capped_60000ms', summary: metrics, time_series: [{ ...metrics, bucket_start: start - 3_600_000 }] });
      if (['/internal/v1/plugins', '/internal/v1/upstreams', '/internal/v1/model-routes', '/internal/v1/provider-groups'].includes(url.pathname)) return json([]);
      unexpected.push(url.pathname); return route.abort();
    });
    await page.addInitScript(() => {
      if (!localStorage.getItem('mtc-locale')) localStorage.setItem('mtc-locale', 'zh-CN');
      localStorage.setItem('mtc.operator.service-credential.v1', 'synthetic-browser-only');
      // An idle synthetic SSE stream retains the production live-state path.
      const original = window.fetch.bind(window);
      window.fetch = (input, init) => {
        const url = new URL(typeof input === 'string' ? input : input instanceof URL ? input.href : input.url, location.origin);
        if (url.pathname === '/internal/v1/request-events') return Promise.resolve(new Response(new ReadableStream(), { headers: { 'Content-Type': 'text/event-stream' } }));
        return original(input, init);
      };
    });
    await page.goto(`${origin}/operator?view=requests`);
    const heading = page.locator('.request-page-surface .traffic-heading h2');
    await heading.waitFor();
    const totalTokens = page.locator('.request-traffic-metrics .analytics-metric').filter({ has: page.locator('.metric-label', { hasText: '总词元' }) });
    await totalTokens.locator('.metric-value', { hasText: '350' }).waitFor();
    const load = page.locator('.load-more .fui-Button');
    await load.waitFor();
    for (const theme of ['light', 'dark']) for (const width of [390, 1440]) {
      await page.evaluate(value => { document.documentElement.dataset.theme = value; }, theme);
      await page.setViewportSize({ width, height: 1000 });
      await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve(null)))));
      await page.waitForFunction(() => getComputedStyle(document.querySelector('.mtc-fluent-root')!).getPropertyValue('--colorNeutralBackground1').trim() !== '');
      await load.focus();
      const typography = await page.evaluate(() => {
        const provider = getComputedStyle(document.querySelector('.mtc-fluent-root')!);
        const heading = getComputedStyle(document.querySelector('.traffic-heading h2')!);
        const helper = getComputedStyle(document.querySelector('.traffic-heading > div > span')!);
        const button = getComputedStyle(document.querySelector('.load-more .fui-Button')!);
        return { baseFont: provider.fontFamily, headingFont: heading.fontFamily, helperFont: helper.fontFamily,
          headingSize: heading.fontSize, titleToken: provider.getPropertyValue('--fontSizeBase500').trim(),
          helperSize: parseFloat(helper.fontSize), radius: button.borderRadius,
          radiusToken: provider.getPropertyValue('--borderRadiusMedium').trim(),
          overflow: document.documentElement.scrollWidth > innerWidth,
          shell: Boolean(document.querySelector('.product-app .app-stage .app-main-content')) };
      });
      assert.equal(typography.shell, true);
      assert.equal(typography.headingFont, typography.baseFont);
      assert.equal(typography.helperFont, typography.baseFont);
      assert.equal(typography.headingSize, typography.titleToken);
      assert.ok(typography.helperSize >= 14);
      assert.equal(typography.radius, typography.radiusToken);
      assert.equal(typography.overflow, false);
      assert.equal(await load.evaluate(element => element === document.activeElement), true);
      assert.equal(await page.locator('.segmented .fui-ToggleButton[aria-pressed="true"]').count(), 1);
      // Focus below the fold scrolls the viewport. Return to the top before a
      // full-page capture so fixed shell elements are not painted mid-page.
      await page.evaluate(() => window.scrollTo({ top: 0, behavior: 'instant' }));
      await page.screenshot({ path: `${artifacts}/requests-${theme}-${width}.png`, fullPage: true });
    }
    // The same production ChartDataView/Fluent Tab used by the isolated
    // overview fixture, with the actual Application provider and stylesheet
    // graph. Visual keyboard-focus acceptance belongs at this real entry.
    for (const locale of ['en', 'zh-CN']) {
      await page.evaluate(value => localStorage.setItem('mtc-locale', value), locale);
      await page.goto(`${origin}/operator?view=overview`);
      const card = page.locator('.overview-trend-card').first();
      await card.locator('canvas').waitFor();
      const chartTab = card.locator('[role="tab"][id$="-chart-tab"]');
      const dataTab = card.locator('[role="tab"][id$="-data-tab"]');
      for (const theme of ['light', 'dark']) for (const width of [320, 390, 768, 1440]) {
        const label = `production overview ${locale} ${theme} ${width}`;
        await page.evaluate(value => { document.documentElement.dataset.theme = value; }, theme);
        await page.setViewportSize({ width, height: 1000 });
        await chartTab.focus();
        await page.keyboard.press('ArrowRight');
        assert.equal(await dataTab.evaluate(element => element === document.activeElement), true, `${label}: arrow key focuses data tab`);
        await page.waitForFunction(element => element.hasAttribute('data-fui-focus-visible'), await dataTab.elementHandle());
        const focus = await dataTab.evaluate(element => {
          const style = getComputedStyle(element);
          return { outline: style.outline, shadow: style.boxShadow,
            visible: (style.outlineStyle !== 'none' && parseFloat(style.outlineWidth) >= 2 && style.outlineColor !== 'rgba(0, 0, 0, 0)') || style.boxShadow !== 'none' };
        });
        assert.equal(focus.visible, true, `${label}: visible keyboard focus ${JSON.stringify(focus)}`);
        await page.keyboard.press('Space');
        assert.equal(await dataTab.getAttribute('aria-selected'), 'true', `${label}: keyboard opens data`);
        await card.locator('.overview-trend-data').waitFor();
        await dataTab.screenshot({ path: `${artifacts}/overview-tab-${locale}-${theme}-${width}.png` });
        await page.keyboard.press('ArrowLeft');
        await page.keyboard.press('Space');
        assert.equal(await chartTab.getAttribute('aria-selected'), 'true', `${label}: keyboard returns to chart`);
      }
    }
    assert.deepEqual(unexpected, []);
    assert.deepEqual(errors, []);
  } finally { await browser.close(); await server.close(); }
});
