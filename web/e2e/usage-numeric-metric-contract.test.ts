import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { readdir } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

import { chromium } from 'playwright';
import { createServer } from 'vite';

const webRoot = fileURLToPath(new URL('..', import.meta.url));
const viewportWidths = [320, 390, 768, 1024, 1440, 1920, 2560] as const;
const displayValuesByLabel: Record<string, string> = {
  'Total tokens': '3.13T',
  'Generation billing units': '1.25T',
  'Cached tokens': '1.25T',
  'Cache-write tokens': '1.25T',
  'Average TPS': '0.21',
};
const exactValuesByLabel: Record<string, string> = {
  'Total tokens': '3,125,000,000,000',
  'Generation billing units': '1,250,000,000,000',
  'Cached tokens': '1,250,000,000,000',
  'Cache-write tokens': '1,250,000,000,000',
  'Average TPS': '0.208333 TPS',
};

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

test('UsageAnalysis renders the omitted TPS card as a dash with a note, never a fabricated number', { timeout: 30_000 }, async () => {
  const executablePath = await localChromiumExecutable();
  if (!executablePath) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required for the usage numeric metric CI gate');
    return test.skip('a local Chromium runtime is required for UsageAnalysis layout assertions');
  }
  const server = await createServer({ root: webRoot, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0, strictPort: false } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ executablePath, headless: true });
  try {
    const page = await browser.newPage();
    await page.clock.install({ time: new Date('2026-10-02T12:00:00Z') });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/usage-analysis.html`);
    await page.locator('.usage-metrics').waitFor();
    await page.getByRole('button', { name: 'Filter', exact: true }).click();
    const dialog = page.getByRole('dialog');
    await dialog.getByRole('button', { name: 'Last 7 days' }).click();
    await dialog.getByRole('button', { name: 'Apply filters' }).click();
    const tpsCard = page.locator('.usage-metrics .analytics-metric').filter({ hasText: 'Average TPS' });
    await tpsCard.locator('.analytics-metric-note').waitFor();
    assert.equal((await tpsCard.locator('.metric-value').innerText()).trim(), '—');
    assert.equal(await tpsCard.locator('.metric-exact').count(), 0, 'the omitted TPS must not expose an exact numeric value');
    assert.equal(await tpsCard.locator('svg, [data-ratio]').count(), 0, 'the omitted TPS renders neither a trend nor a ratio background');
    assert.match(await tpsCard.locator('.analytics-metric-note').innerText(), /not calculated for long ranges/);
    for (const theme of ['dark', 'light'] as const) {
      await page.evaluate((value) => { document.documentElement.dataset.theme = value; }, theme);
      for (const width of [320, 1440] as const) {
        await page.setViewportSize({ width, height: 900 });
        await page.evaluate(() => new Promise<void>((resolve) => requestAnimationFrame(() => resolve())));
        const layout = await page.evaluate(() => {
          const card = [...document.querySelectorAll<HTMLElement>('.usage-metrics .metric')].find((candidate) => candidate.querySelector('.metric-label')?.textContent === 'Average TPS')!;
          const note = card.querySelector<HTMLElement>('.analytics-metric-note')!;
          return {
            documentClientWidth: document.documentElement.clientWidth,
            documentScrollWidth: document.documentElement.scrollWidth,
            metricClientWidth: card.clientWidth,
            metricScrollWidth: card.scrollWidth,
            noteVisible: note.getClientRects().length > 0,
          };
        });
        assert.equal(layout.noteVisible, true, `${theme} ${width}px keeps the omission note visible`);
        assert.ok(layout.metricScrollWidth <= layout.metricClientWidth, `${theme} ${width}px omitted TPS card must not overflow`);
        assert.ok(layout.documentScrollWidth <= layout.documentClientWidth, `${theme} ${width}px UsageAnalysis page must not overflow`);
      }
    }
  } finally {
    await browser.close();
    await server.close();
  }
});

test('UsageAnalysis keeps every rendered NumericMetric exact value on one readable line', { timeout: 30_000 }, async () => {
  const executablePath = await localChromiumExecutable();
  if (!executablePath) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required for the usage numeric metric CI gate');
    return test.skip('a local Chromium runtime is required for UsageAnalysis layout assertions');
  }
  const server = await createServer({ root: webRoot, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0, strictPort: false } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ executablePath, headless: true });
  try {
    const page = await browser.newPage();
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/usage-analysis.html`);
    await page.locator('.usage-metrics .metric-exact').first().waitFor();
    await page.locator('.usage-metrics .metric-label').getByText('Average TPS', { exact: true }).focus();
    const tpsDefinition = page.getByRole('tooltip').filter({ hasText: 'Provider-reported output tokens of successful, non-compaction requests' });
    await tpsDefinition.waitFor();
    assert.match(await tpsDefinition.innerText(), /Archived records without sample provenance are excluded/);
    assert.match(await tpsDefinition.innerText(), /Eligible samples: 1,000/, 'the card description must expose the eligible sample count');
    await page.keyboard.press('Escape');

    for (const theme of ['dark', 'light'] as const) {
      await page.evaluate((value) => { document.documentElement.dataset.theme = value; }, theme);
      for (const width of viewportWidths) {
        await page.setViewportSize({ width, height: 900 });
        await page.evaluate(() => new Promise<void>((resolve) => requestAnimationFrame(() => resolve())));
        const layout = await page.evaluate(() => {
          const grid = document.querySelector<HTMLElement>('.usage-metrics')!;
          const cards = [...grid.querySelectorAll<HTMLElement>('.metric')];
          return {
            documentClientWidth: document.documentElement.clientWidth,
            documentScrollWidth: document.documentElement.scrollWidth,
            gridClientWidth: grid.clientWidth,
            gridScrollWidth: grid.scrollWidth,
            cards: cards.map((card) => {
              const value = card.querySelector<HTMLElement>('.metric-value')!;
              const exact = card.querySelector<HTMLElement>('.metric-exact');
              const range = document.createRange();
              if (exact) range.selectNodeContents(exact);
              return {
                exactFontSize: exact ? Number.parseFloat(getComputedStyle(exact).fontSize) : undefined,
                exactLineCount: exact ? range.getClientRects().length : undefined,
                exactText: exact?.textContent,
                exactTitle: exact?.getAttribute('title'),
                label: card.querySelector('.metric-label')?.textContent,
                metricClientWidth: card.clientWidth,
                metricScrollWidth: card.scrollWidth,
                valueFontSize: Number.parseFloat(getComputedStyle(value).fontSize),
                valueLineHeight: Number.parseFloat(getComputedStyle(value).lineHeight),
              };
            }),
          };
        });

        const numericCards = layout.cards.filter((card) => card.exactText !== undefined);
        assert.equal(numericCards.length, 5, `${theme} ${width}px fixture must render all UsageAnalysis NumericMetric cards`);
        assert.equal(layout.cards.some(card => card.label === 'P95 TPS'), false, 'bucket averages cannot establish request TPS percentiles');
        for (const card of numericCards) {
          const expectedDisplay = card.label ? displayValuesByLabel[card.label] : undefined;
          const expectedExact = card.label ? exactValuesByLabel[card.label] : undefined;
          assert.ok(expectedDisplay && expectedExact, `${theme} ${width}px NumericMetric must retain its expected UsageAnalysis label`);
          assert.equal(card.exactText, expectedDisplay, `${theme} ${width}px ${card.label} NumericMetric must expose its localized compact value`);
          assert.equal(card.exactTitle, expectedExact, `${theme} ${width}px ${card.label} NumericMetric must retain full precision in its tooltip`);
          assert.equal(card.exactLineCount, 1, `${theme} ${width}px NumericMetric display value must not wrap`);
          assert.equal(card.exactFontSize, card.valueFontSize, `${theme} ${width}px NumericMetric display value must remain primary`);
          assert.ok(card.valueLineHeight >= card.valueFontSize * 1.1, `${theme} ${width}px NumericMetric line-height must remain readable`);
        }
        for (const card of layout.cards) assert.ok(card.metricScrollWidth <= card.metricClientWidth, `${theme} ${width}px UsageAnalysis metric card must not overflow`);
        assert.ok(layout.gridScrollWidth <= layout.gridClientWidth, `${theme} ${width}px UsageAnalysis metric grid must not overflow`);
        assert.ok(layout.documentScrollWidth <= layout.documentClientWidth, `${theme} ${width}px UsageAnalysis page must not overflow`);
      }
    }
  } finally {
    await browser.close();
    await server.close();
  }
});
