import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer as createServer } from './support/isolated-vite-server.js';
import { fixtureAssets } from './support/fixture-assets.js';

type Rgba = [number, number, number, number];
function parseColor(value: string): Rgba {
  const channels = value.match(/[\d.]+/g);
  assert.ok(channels && channels.length >= 3, `expected a computed color, got ${value}`);
  return [Number(channels[0]), Number(channels[1]), Number(channels[2]), channels.length > 3 ? Number(channels[3]) : 1];
}
/** Layers are computed backgrounds ordered from the element outward. Composite
 * every translucent layer over the nearest opaque ancestor (defaulting to the
 * white canvas) so contrast uses the actually rendered background. */
function effectiveBackground(layers: string[]): Rgba {
  let baseIndex = layers.length;
  let result: Rgba = [255, 255, 255, 1];
  for (let index = 0; index < layers.length; index += 1) {
    const candidate = parseColor(layers[index]);
    if (candidate[3] >= 1) { result = candidate; baseIndex = index; break; }
  }
  for (let index = baseIndex - 1; index >= 0; index -= 1) {
    const layer = parseColor(layers[index]);
    result = [0, 1, 2].map(channel => layer[3] * layer[channel as 0 | 1 | 2] + (1 - layer[3]) * result[channel as 0 | 1 | 2]) as unknown as Rgba;
  }
  return result;
}
function luminance([r, g, b]: Rgba) {
  const linear = [r, g, b].map(v => { const c = v / 255; return c <= 0.03928 ? c / 12.92 : Math.pow((c + 0.055) / 1.055, 2.4); });
  return 0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2];
}
function contrast(foreground: string, background: Rgba) {
  const [high, low] = [luminance(parseColor(foreground)), luminance(background)].sort((a, b) => b - a);
  return (high + 0.05) / (low + 0.05);
}

const job = {
  job_id: '019f0000-0000-7000-8000-000000000099',
  created_at: 1_700_000_000_000,
  updated_at: 1_700_000_000_000,
  completed_at: 1_700_000_100_000,
  model: 'fixture-code-contrast-model',
  driver: 'http-json',
  billing_unit: 'job',
  status: 'succeeded',
  upstream_job_id: null,
  estimated_units: 1,
  billed_units: 1,
  cost: '0.29',
  error_code: null,
  result: null,
  assets: [],
  tenant_external_id: 'default',
  key_id: '019f0000-0000-7000-8000-000000000098',
  key_alias: 'Fixture key',
  currency: 'USD',
};

// Render the production index/main/Application/AppShell/Operator graph, not a
// hand-assembled page fixture. Only network data is synthetic and core-owned.
test('production generations table code keeps AA contrast, stays monospace and is not disguised as a link', { timeout: 90_000 }, async () => {
  const root = fileURLToPath(new URL('..', import.meta.url));
  const server = await createServer({ root, configFile: false, plugins: [fixtureAssets()], logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const origin = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  const artifacts = `${root}/e2e-artifacts/ui-system/code-contrast`;
  await mkdir(artifacts, { recursive: true });
  try {
    const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
    const unexpected: string[] = [], errors: string[] = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.route('**/*', async route => {
      const request = route.request(), url = new URL(request.url());
      if (url.origin !== origin) { unexpected.push(url.origin); return route.abort(); }
      if (!url.pathname.startsWith('/internal/')) return route.continue();
      const json = (data: unknown) => route.fulfill({ contentType: 'application/json', body: JSON.stringify(data) });
      if (request.method() !== 'GET') { unexpected.push(`${request.method()} ${url.pathname}`); return route.abort(); }
      if (url.pathname === '/internal/v1/tenants') return json([{ id: 'synthetic', external_id: 'default', name: 'Design acceptance', created_at: 1, updated_at: 1 }]);
      if (url.pathname === '/internal/v1/generations') return json([job]);
      if (url.pathname === '/internal/v1/plugins') return json([]);
      unexpected.push(url.pathname); return route.abort();
    });
    await page.addInitScript(() => {
      if (!localStorage.getItem('mtc-locale')) localStorage.setItem('mtc-locale', 'zh-CN');
      localStorage.setItem('mtc.operator.service-credential.v1', 'synthetic-browser-only');
    });
    await page.goto(`${origin}/operator?view=generations`);
    const code = page.locator('.operator-generations td code', { hasText: 'fixture-code-contrast-model' });
    await code.waitFor();
    for (const theme of ['light', 'dark']) {
      await page.evaluate(value => { document.documentElement.dataset.theme = value; }, theme);
      await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve(null)))));
      await page.waitForFunction(() => getComputedStyle(document.querySelector('.mtc-fluent-root')!).getPropertyValue('--colorNeutralBackground1').trim() !== '');
      const sample = await code.evaluate(el => {
        const layers: string[] = [];
        let node: HTMLElement | null = el as HTMLElement;
        while (node) { layers.push(getComputedStyle(node).backgroundColor); node = node.parentElement; }
        const style = getComputedStyle(el);
        return { color: style.color, layers, fontFamily: style.fontFamily, decoration: style.textDecorationLine, cursor: style.cursor };
      });
      const background = effectiveBackground(sample.layers);
      const ratio = contrast(sample.color, background);
      assert.ok(ratio >= 4.5, `${theme}: code ${sample.color} on composited rgb(${background.slice(0, 3).map(Math.round).join(',')}) = ${ratio.toFixed(2)}:1, below the 4.5:1 normal-text floor`);
      assert.match(sample.fontFamily, /mono/i, `${theme}: technical identifiers stay monospace`);
      assert.equal(sample.decoration.includes('underline'), false, `${theme}: non-link code must not borrow link underline affordance`);
      assert.equal(sample.cursor, 'auto', `${theme}: non-link code keeps the text cursor`);
      await page.screenshot({ path: `${artifacts}/code-contrast-${theme}-1440.png` });
    }
    assert.deepEqual(unexpected, []);
    assert.deepEqual(errors, []);
  } finally { await browser.close(); await server.close(); }
});
