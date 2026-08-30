import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { readdir, readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

import { chromium } from 'playwright';

const sharedStylesPath = fileURLToPath(new URL('../src/styles.css', import.meta.url));
const themeStylesPath = fileURLToPath(new URL('../src/theme.css', import.meta.url));
const metricStylesPath = fileURLToPath(new URL('../src/styles/metrics.css', import.meta.url));
const viewportWidths = [320, 390, 768, 1024, 1440, 1920, 2560] as const;

async function localChromiumExecutable() {
  const defaultExecutable = chromium.executablePath();
  if (existsSync(defaultExecutable)) return defaultExecutable;
  const workspaceUserCache = fileURLToPath(new URL('../../../../.cache/ms-playwright', import.meta.url));
  const installations = await readdir(workspaceUserCache, { withFileTypes: true }).catch(() => []);
  for (const installation of installations) {
    if (!installation.isDirectory() || !installation.name.startsWith('chromium-')) continue;
    const executable = join(workspaceUserCache, installation.name, 'chrome-linux64', 'chrome');
    if (existsSync(executable)) return executable;
  }
  return undefined;
}

test('metric stylesheet owns label and value typography through explicit classes', async () => {
  const css = await readFile(metricStylesPath, 'utf8');
  const sharedCss = await readFile(sharedStylesPath, 'utf8');
  const components = await readFile(new URL('../src/components.tsx', import.meta.url), 'utf8');

  assert.match(components, /className="metric-label"/);
  assert.match(components, /className="metric-value"/);
  assert.match(css, /\.metric > \.metric-label/);
  assert.match(css, /\.metric > \.metric-value/);
  assert.match(css, /font-variant-numeric:\s*lining-nums tabular-nums/);
  assert.doesNotMatch(css, /Georgia|font-size:\s*11px/);
  assert.doesNotMatch(sharedCss, /\.metric span\s*\{/);
});

test('metric typography remains readable and overflow-free at supported widths in both themes', {
  timeout: 30_000,
}, async () => {
  const executablePath = await localChromiumExecutable();
  if (!executablePath) return test.skip('a local Chromium runtime is required for computed-style assertions');
  const browser = await chromium.launch({ executablePath, headless: true });
  try {
    const page = await browser.newPage();
    await page.setContent(`
      <section class="metrics">
        <article class="metric positive">
          <span class="metric-label">Token usage</span>
          <strong class="metric-value">
            <span class="metric-number">
              <span class="metric-exact">1,250,000,000,000</span>
              <small class="metric-compact" aria-hidden="true">1.25T</small>
            </span>
          </strong>
        </article>
        <article class="metric">
          <span class="metric-label">总请求</span>
          <strong class="metric-value">350,588</strong>
        </article>
      </section>
    `);
    await page.addStyleTag({ path: sharedStylesPath });
    await page.addStyleTag({ path: themeStylesPath });
    await page.addStyleTag({ path: metricStylesPath });

    for (const theme of ['dark', 'light'] as const) {
      await page.evaluate((value) => { document.documentElement.dataset.theme = value; }, theme);
      for (const width of viewportWidths) {
        await page.setViewportSize({ width, height: 720 });
        const styles = await page.locator('.metric').first().evaluate((metric) => {
          const label = metric.querySelector<HTMLElement>('.metric-label')!;
          const value = metric.querySelector<HTMLElement>('.metric-value')!;
          const exact = metric.querySelector<HTMLElement>('.metric-exact')!;
          const compact = metric.querySelector<HTMLElement>('.metric-compact')!;
          const labelStyle = getComputedStyle(label);
          const valueStyle = getComputedStyle(value);
          const compactStyle = getComputedStyle(compact);
          const grid = metric.closest<HTMLElement>('.metrics')!;
          return {
            compactFontSize: Number.parseFloat(compactStyle.fontSize),
            exactFontSize: Number.parseFloat(getComputedStyle(exact).fontSize),
            fontFamily: valueStyle.fontFamily,
            fontVariantNumeric: valueStyle.fontVariantNumeric,
            gridClientWidth: grid.clientWidth,
            gridScrollWidth: grid.scrollWidth,
            labelFontSize: Number.parseFloat(labelStyle.fontSize),
            labelLineHeight: Number.parseFloat(labelStyle.lineHeight),
            metricClientWidth: metric.clientWidth,
            metricScrollWidth: metric.scrollWidth,
            valueFontSize: Number.parseFloat(valueStyle.fontSize),
            valueLineHeight: Number.parseFloat(valueStyle.lineHeight),
          };
        });

        assert.ok(styles.labelFontSize >= 12, `${theme} ${width}px label must be at least 12px`);
        assert.ok(styles.labelLineHeight >= styles.labelFontSize * 1.3, `${theme} ${width}px label line-height must be stable`);
        assert.ok(styles.valueFontSize >= 24, `${theme} ${width}px primary value must be at least 24px`);
        assert.ok(styles.valueLineHeight >= styles.valueFontSize * 1.1, `${theme} ${width}px value line-height must be stable`);
        assert.equal(styles.exactFontSize, styles.valueFontSize, `${theme} ${width}px exact value must remain primary`);
        assert.ok(styles.compactFontSize < styles.exactFontSize, `${theme} ${width}px compact value must remain secondary`);
        assert.doesNotMatch(styles.fontFamily, /Georgia/i, `${theme} ${width}px dynamic values must not use Georgia`);
        assert.match(styles.fontVariantNumeric, /tabular-nums/);
        assert.match(styles.fontVariantNumeric, /lining-nums/);
        assert.ok(styles.gridScrollWidth <= styles.gridClientWidth, `${theme} ${width}px metric grid must not overflow`);
        assert.ok(styles.metricScrollWidth <= styles.metricClientWidth, `${theme} ${width}px metric card must not overflow`);
      }
    }
  } finally {
    await browser.close();
  }
});
