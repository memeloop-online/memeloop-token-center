import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer as createServer } from './support/isolated-vite-server.js';
import { fixtureAssets } from './support/fixture-assets.js';

// Render the production index/main/Application/AppShell/Operator graph, not a
// hand-assembled page fixture. Only network data is synthetic and core-owned.
test('production AppShell typography and actions share Fluent tokens in both themes and sizes', { timeout: 60_000 }, async () => {
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
    const base = { request_id: 'synthetic-running', created_at: start, completed_at: null, model: 'sample-model', protocol: 'openai', status_code: null, duration_ms: null, input_tokens: 0, output_tokens: 0, cost: '0', error_code: null };
    const requests = [base,
      { ...base, request_id: 'synthetic-success', created_at: start + 30_000, completed_at: start + 50_000, status_code: 200, duration_ms: 20_000 },
      { ...base, request_id: 'synthetic-failure', created_at: start + 60_000, completed_at: start + 90_000, status_code: 502, duration_ms: 30_000 },
    ];
    await page.route('**/*', async route => {
      const request = route.request(), url = new URL(request.url());
      if (url.origin !== origin) { unexpected.push(url.origin); return route.abort(); }
      if (!url.pathname.startsWith('/internal/')) return route.continue();
      const json = (data: unknown) => route.fulfill({ contentType: 'application/json', body: JSON.stringify(data) });
      if (url.pathname === '/internal/v1/requests/query' && request.method() === 'POST') return json({ requests, next_cursor: 'synthetic-older' });
      if (request.method() !== 'GET') { unexpected.push(`${request.method()} ${url.pathname}`); return route.abort(); }
      if (url.pathname === '/internal/v1/tenants') return json([{ id: 'synthetic', external_id: 'default', name: 'Design acceptance', created_at: 1, updated_at: 1 }]);
      if (['/internal/v1/plugins', '/internal/v1/upstreams', '/internal/v1/model-routes', '/internal/v1/provider-groups'].includes(url.pathname)) return json([]);
      unexpected.push(url.pathname); return route.abort();
    });
    await page.addInitScript(() => {
      localStorage.setItem('mtc-locale', 'zh-CN');
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
    await page.waitForFunction(() => document.querySelector('.request-traffic-metrics .metric-value')?.textContent === '3');
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
      await page.screenshot({ path: `${artifacts}/requests-${theme}-${width}.png`, fullPage: true });
    }
    assert.deepEqual(unexpected, []);
    assert.deepEqual(errors, []);
  } finally { await browser.close(); await server.close(); }
});
