import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

import { chromium } from 'playwright';
import { localChromiumExecutable } from './support/browser-executable.js';

const sharedStylesPath = fileURLToPath(new URL('../src/styles.css', import.meta.url));
const themeStylesPath = fileURLToPath(new URL('../src/theme.css', import.meta.url));
const metricStylesPath = fileURLToPath(new URL('../src/styles/metrics.css', import.meta.url));
const viewportWidths = [320, 390, 768, 1024, 1440, 1920, 2560] as const;

test('metric components expose explicit label and value classes without a legacy span override', async () => {
  const sharedCss = await readFile(sharedStylesPath, 'utf8');
  const components = await readFile(new URL('../src/components.tsx', import.meta.url), 'utf8');

  assert.match(components, /className="metric-label"/);
  assert.match(components, /className="metric-value"/);
  assert.doesNotMatch(sharedCss, /\.metric span\s*\{/);
});

test('metric typography remains readable and overflow-free at supported widths in both themes', {
  timeout: 30_000,
}, async (context) => {
  const executablePath = await localChromiumExecutable();
  if (!executablePath) return context.skip('a local Chromium runtime is required for computed-style assertions');
  const browser = await chromium.launch({ executablePath, headless: true });
  try {
    const page = await browser.newPage();
    await page.setContent(['positive', '', 'usage'].map((variant) => `
      <section class="metrics ${variant === 'usage' ? 'usage-metrics' : ''}">
        <article class="metric ${variant === 'positive' ? 'positive' : ''}">
          <span class="metric-label">Token usage</span>
          <strong class="metric-value">
            <span class="metric-number">
              <span class="metric-exact">1,250,000,000,000</span>
              <small class="metric-compact" aria-hidden="true">1.25T</small>
            </span>
          </strong>
        </article>
      </section>
    `).join(''));
    await page.addStyleTag({ path: sharedStylesPath });
    await page.addStyleTag({ path: themeStylesPath });
    await page.addStyleTag({ path: metricStylesPath });

    for (const theme of ['dark', 'light'] as const) {
      await page.evaluate((value) => { document.documentElement.dataset.theme = value; }, theme);
      for (const width of viewportWidths) {
        await page.setViewportSize({ width, height: 720 });
        const metrics = page.locator('.metric');
        assert.equal(await metrics.count(), 3);
        for (const metric of await metrics.all()) {
          const styles = await metric.evaluate((metric) => {
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
          assert.ok(styles.compactFontSize >= 12, `${theme} ${width}px compact value must be at least 12px`);
          assert.ok(styles.compactFontSize < styles.exactFontSize, `${theme} ${width}px compact value must remain secondary`);
          assert.doesNotMatch(styles.fontFamily, /Georgia/i, `${theme} ${width}px dynamic values must not use Georgia`);
          assert.match(styles.fontVariantNumeric, /tabular-nums/);
          assert.match(styles.fontVariantNumeric, /lining-nums/);
          assert.ok(styles.gridScrollWidth <= styles.gridClientWidth, `${theme} ${width}px metric grid must not overflow`);
          assert.ok(styles.metricScrollWidth <= styles.metricClientWidth, `${theme} ${width}px metric card must not overflow`);
        }
      }
    }
  } finally {
    await browser.close();
  }
});
