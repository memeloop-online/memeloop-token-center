import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { mkdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer as createServer } from './support/isolated-vite-server.js';

function parseRgb(value: string): [number, number, number] {
  const channels = value.match(/[\d.]+/g);
  assert.ok(channels && channels.length >= 3, `expected a computed rgb color, got ${value}`);
  return [Number(channels[0]), Number(channels[1]), Number(channels[2])];
}
function luminance([r, g, b]: [number, number, number]) {
  const linear = [r, g, b].map(v => { const c = v / 255; return c <= 0.03928 ? c / 12.92 : Math.pow((c + 0.055) / 1.055, 2.4); });
  return 0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2];
}
function contrast(foreground: string, background: string) {
  const [high, low] = [luminance(parseRgb(foreground)), luminance(parseRgb(background))].sort((a, b) => b - a);
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
  tenant_external_id: 'alpha',
  key_id: '019f0000-0000-7000-8000-000000000098',
  key_alias: 'Fixture key',
  currency: 'USD',
};

test('shared table code keeps AA text contrast, stays monospace and is not disguised as a link', { timeout: 45_000 }, async context => {
  const executablePath = process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH ?? chromium.executablePath();
  if (!existsSync(executablePath)) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required');
    return context.skip('Chromium runtime is required');
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer!.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ executablePath, headless: true });
  const artifacts = fileURLToPath(new URL('../e2e-artifacts/ui-system/code-contrast/', import.meta.url));
  await mkdir(artifacts, { recursive: true });
  try {
    const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
    await page.route('**/internal/v1/generations?**', route => route.fulfill({ json: [job] }));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/generation-workspace.html`);
    const code = page.locator('.operator-generations td code', { hasText: 'fixture-code-contrast-model' });
    await code.waitFor();
    for (const theme of ['light', 'dark']) {
      await page.evaluate(value => { document.documentElement.dataset.theme = value; }, theme);
      await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
      const sample = await code.evaluate(el => {
        let node: HTMLElement | null = el as HTMLElement;
        let background = 'transparent';
        while (node) {
          const candidate = getComputedStyle(node).backgroundColor;
          if (candidate !== 'transparent' && !candidate.startsWith('rgba(0, 0, 0, 0')) { background = candidate; break; }
          node = node.parentElement;
        }
        const style = getComputedStyle(el);
        return { color: style.color, background, fontFamily: style.fontFamily, decoration: style.textDecorationLine, cursor: style.cursor };
      });
      const ratio = contrast(sample.color, sample.background);
      assert.ok(ratio >= 4.5, `${theme}: code ${sample.color} on ${sample.background} = ${ratio.toFixed(2)}:1, below the 4.5:1 normal-text floor`);
      assert.match(sample.fontFamily, /mono/i, `${theme}: technical identifiers stay monospace`);
      assert.equal(sample.decoration.includes('underline'), false, `${theme}: non-link code must not borrow link underline affordance`);
      assert.equal(sample.cursor, 'auto', `${theme}: non-link code keeps the text cursor`);
      await page.screenshot({ path: `${artifacts}code-contrast-${theme}-1440.png` });
    }
  } finally { await browser.close(); await server.close(); }
});
