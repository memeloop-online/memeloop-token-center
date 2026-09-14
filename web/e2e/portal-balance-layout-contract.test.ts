import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import test from 'node:test';
import { chromium } from 'playwright';

test('account balance keeps a readable width in both themes and mobile layouts', async context => {
  const executablePath = process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH ?? chromium.executablePath();
  if (!existsSync(executablePath)) return context.skip('Chromium runtime is required');
  const browser = await chromium.launch({ executablePath, headless: true });
  try {
    const page = await browser.newPage();
    const css = ['styles.css', 'theme.css', 'styles/metrics.css', 'self/SelfPortal.css'].map(path => readFileSync(new URL(`../src/${path}`, import.meta.url), 'utf8')).join('\n');
    await page.setContent(`<style>${css}</style><main class="self-overview"><article class="panel key-summary self-account-summary"><div><h2>Example account</h2></div><article class="metric"><span class="metric-label">可用余额 (USD)</span><strong class="metric-value"><span>9.22万亿 USD</span></strong></article></article></main>`);
    for (const theme of ['light', 'dark']) for (const width of [390, 1440]) {
      await page.setViewportSize({ width, height: 1000 });
      await page.evaluate(theme => { document.documentElement.dataset.theme = theme; }, theme);
      const layout = await page.locator('.self-account-summary').evaluate(element => {
        const value = element.querySelector('.metric-value')!;
        const range = document.createRange();
        range.selectNodeContents(value);
        return { width: value.getBoundingClientRect().width, lines: range.getClientRects().length, shadow: getComputedStyle(element).boxShadow };
      });
      assert.ok(layout.width > 200, `${theme} ${width}: inline-size containers must not collapse in the account row`);
      assert.ok(layout.lines <= 2, `${theme} ${width}: compact balance must not wrap character by character`);
      assert.equal(layout.shadow, 'none', `${theme} ${width}: theme panel rule must not restore shadow`);
    }
  } finally { await browser.close(); }
});
