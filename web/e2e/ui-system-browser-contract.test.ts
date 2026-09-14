import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { mkdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer as createServer } from './support/isolated-vite-server.js';
import { fixtureAssets } from './support/fixture-assets.js';

const routes = [
  { name: 'operator-overview', ready: '.overview-trend-card canvas', surface: '.overview-trend-card', action: '.overview-trend-data > summary' },
  { name: 'request-diagnostics', ready: '.request-diagnostics', surface: '.request-diagnostics', action: '.request-diagnostic-session' },
  { name: 'upstream-availability', ready: '.provider-availability', surface: '.provider-availability', action: '.provider-attempt-link' },
  { name: 'operator-form-sections', ready: '.mtc-form-section', surface: '.mtc-form-section, .form-journey-disclosure', action: '.form-journey-disclosure button[aria-expanded]' },
] as const;

type SurfaceLayout = { client: number; scroll: number; left: number; right: number };
type LayoutStability = { observer: ResizeObserver; generation: number; checkedGeneration: number; stableFrames: number };

declare global {
  interface Window { __uiSystemLayoutStability?: Record<string, LayoutStability> }
}

async function measuredLayout(page: import('playwright').Page, surface: string): Promise<{ documentClient: number; documentScroll: number; surfaces: SurfaceLayout[] }> {
  return page.evaluate((selector) => ({
    documentClient: document.documentElement.clientWidth,
    documentScroll: document.documentElement.scrollWidth,
    surfaces: [...document.querySelectorAll<HTMLElement>(selector)].map((element) => {
      const bounds = element.getBoundingClientRect();
      return { client: element.clientWidth, scroll: element.scrollWidth, left: bounds.left, right: bounds.right };
    }),
  }), surface);
}

async function waitForStableLayout(page: import('playwright').Page, surface: string, label: string) {
  try {
    await page.waitForFunction((selector) => {
      const surfaces = [...document.querySelectorAll<HTMLElement>(selector)];
      const layouts = window.__uiSystemLayoutStability ??= {};
      const stability = layouts[selector] ??= (() => {
        const state: LayoutStability = { observer: undefined as never, generation: 0, checkedGeneration: -1, stableFrames: 0 };
        state.observer = new ResizeObserver(() => { state.generation += 1; });
        state.observer.observe(document.documentElement);
        surfaces.forEach((element) => state.observer.observe(element));
        return state;
      })();
      const contained = surfaces.length > 0
        && document.documentElement.scrollWidth <= innerWidth
        && surfaces.every((element) => {
          const bounds = element.getBoundingClientRect();
          return element.scrollWidth <= element.clientWidth && bounds.left >= 0 && bounds.right <= innerWidth;
        });
      if (!contained) {
        stability.stableFrames = 0;
        return false;
      }
      if (stability.checkedGeneration !== stability.generation) {
        stability.checkedGeneration = stability.generation;
        stability.stableFrames = 1;
        return false;
      }
      stability.stableFrames += 1;
      return stability.stableFrames >= 2;
    }, surface);
  } catch {
    const layout = await measuredLayout(page, surface);
    assert.fail(`${label}: layout did not stabilize within the existing browser deadline: ${JSON.stringify(layout)}`);
  } finally {
    await page.evaluate((selector) => {
      const stability = window.__uiSystemLayoutStability?.[selector];
      stability?.observer.disconnect();
      if (window.__uiSystemLayoutStability) delete window.__uiSystemLayoutStability[selector];
    }, surface).catch(() => undefined);
  }
  return measuredLayout(page, surface);
}

test('shared surfaces contain long content and retain keyboard actions across locale/theme/viewport', { timeout: 120_000 }, async (context) => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required for UI system contracts');
    context.skip('Chromium required'); return;
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, plugins: [fixtureAssets()], logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  const artifacts = fileURLToPath(new URL('../e2e-artifacts/ui-system/', import.meta.url));
  await mkdir(artifacts, { recursive: true });
  try {
    for (const route of routes) for (const locale of ['en', 'zh-CN']) {
      const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
      page.setDefaultTimeout(5_000);
      if (route.name === 'request-diagnostics') await page.context().grantPermissions(['clipboard-read', 'clipboard-write'], { origin: `http://127.0.0.1:${address.port}` });
      const errors: string[] = [];
      page.on('pageerror', error => errors.push(error.message));
      // Fixed wall time keeps credential expiry meaningful; timers/RAF run normally.
      await page.clock.setFixedTime(new Date('2026-09-09T12:00:00Z'));
      await page.addInitScript(value => localStorage.setItem('mtc-locale', value), locale);
      await page.route('**/*', async request => {
        const url = new URL(request.request().url());
        if (url.hostname !== '127.0.0.1') {
          errors.push(`Unexpected external fixture request: ${url.origin}`);
          await request.abort(); return;
        }
        await request.continue();
      });
      await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/${route.name}.html?long-tokens=1`);
      await page.locator(route.ready).first().waitFor();
      if (route.name === 'operator-overview') {
        await page.getByRole('button', { name: 'Switch tenant', exact: true }).click();
        await page.getByText('beta-current-model', { exact: true }).waitFor();
        const logo = page.locator('.brand-mark img');
        await logo.evaluate(async element => { await (element as HTMLImageElement).decode(); });
        assert.equal(await logo.evaluate(element => (element as HTMLImageElement).naturalWidth > 0), true, 'fixture serves the production asset; no product fallback needed');
      }
      for (const theme of ['light', 'dark']) for (const width of [320, 390, 768, 1440]) {
        const label = `${route.name} ${locale} ${theme} ${width}`;
        await page.evaluate(value => { document.documentElement.dataset.theme = value; }, theme);
        await page.setViewportSize({ width, height: 900 });
        await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
        const layout = await waitForStableLayout(page, route.surface, label);
        const surfaces = layout.surfaces;
        assert.ok(surfaces.length > 0, label);
        for (const surface of surfaces) {
          assert.ok(surface.scroll <= surface.client, `${label}: component overflow, not merely hidden by body`);
          assert.ok(surface.left >= 0 && surface.right <= width, `${label}: surface inside viewport`);
        }
        assert.ok(layout.documentScroll <= layout.documentClient, `${label}: document overflow ${layout.documentScroll}/${layout.documentClient}`);
        const action = page.locator(route.action).first();
        await page.keyboard.press('Tab');
        await action.focus();
        assert.equal(await action.evaluate(element => element === document.activeElement), true, `${label}: keyboard focus`);
        assert.equal(await action.evaluate(element => getComputedStyle(element).outlineStyle !== 'none'), true, `${label}: visible focus ring`);
        if (width <= 390 && route.name !== 'operator-overview') {
          assert.ok(await action.evaluate(element => element.getBoundingClientRect().height >= 44), `${label}: touch action size`);
        }
        if (route.name === 'operator-form-sections') {
          await page.keyboard.press('Enter');
          assert.equal(await action.getAttribute('aria-expanded'), 'true', `${label}: keyboard disclosure`);
          await page.keyboard.press('Enter');
          assert.equal(await action.getAttribute('aria-expanded'), 'false', `${label}: keyboard collapse`);
          if (width <= 390) assert.ok(await page.locator('.rjsf > button[type="submit"]').evaluateAll(elements => elements.length > 0 && elements.every(element => element.getBoundingClientRect().height >= 44)), `${label}: every save target`);
        }
        if (route.name === 'request-diagnostics') {
          assert.equal(await action.textContent(), 'session_'.repeat(40), `${label}: full label retained`);
          const sessionCell = await page.locator('.request-session-cell').first().evaluate(cell => {
            const bounds = cell.getBoundingClientRect();
            return {
              client: cell.clientWidth,
              scroll: cell.scrollWidth,
              children: [...cell.children].map(child => {
                const childBounds = child.getBoundingClientRect();
                return { left: childBounds.left, right: childBounds.right, text: child.textContent };
              }),
              left: bounds.left,
              right: bounds.right,
            };
          });
          assert.ok(sessionCell.scroll <= sessionCell.client, `${label}: session cell content contained`);
          assert.ok(sessionCell.children.length >= 2, `${label}: session label and metadata retained`);
          for (const child of sessionCell.children) {
            assert.ok(child.left >= sessionCell.left && child.right <= sessionCell.right, `${label}: session child inside its table cell`);
          }
          assert.equal(await page.locator('.request-session-cell details, .request-session-cell summary').count(), 0, `${label}: metadata uses no disclosure triangle`);
          const metadataToggle = page.getByRole('button', { name: locale === 'en' ? 'Task and agent details' : '任务与代理详情', exact: true }).first();
          const metadata = page.locator('.request-session-metadata-popover').first();
          const expectedMetadata = `turn · ${'agent_'.repeat(40)}`;
          const collapsedHeight = await page.locator('tbody tr').first().evaluate(row => row.getBoundingClientRect().height);
          assert.equal(await metadata.count(), 0, `${label}: secondary metadata starts outside the layout`);
          await metadataToggle.focus();
          assert.equal(await metadataToggle.evaluate(element => getComputedStyle(element).outlineStyle !== 'none'), true, `${label}: metadata focus visible`);
          await page.keyboard.press('Enter');
          assert.equal(await metadata.evaluate(element => element.matches(':popover-open')), true, `${label}: keyboard opens native popover`);
          assert.equal(await metadataToggle.getAttribute('aria-expanded'), 'true', `${label}: trigger exposes popover state`);
          assert.equal(await metadata.getAttribute('aria-modal'), 'false', `${label}: metadata does not block the request page`);
          assert.equal(await metadata.locator('code').textContent(), expectedMetadata, `${label}: full metadata available without hover`);
          const openHeight = await page.locator('tbody tr').first().evaluate(row => row.getBoundingClientRect().height);
          assert.equal(openHeight, collapsedHeight, `${label}: popover does not expand the row`);
          const metadataBounds = await metadata.evaluate(element => { const bounds = element.getBoundingClientRect(); return { left: bounds.left, right: bounds.right }; });
          assert.ok(metadataBounds.left >= 0 && metadataBounds.right <= width, `${label}: metadata popover fits viewport`);
          await page.keyboard.press('Tab');
          const metadataCopy = metadata.getByRole('button', { name: locale === 'en' ? 'Copy' : '复制', exact: true });
          assert.equal(await metadataCopy.evaluate(element => document.activeElement === element), true, `${label}: copy control follows trigger in keyboard order`);
          await page.keyboard.press('Enter');
          assert.equal(await page.evaluate(() => navigator.clipboard.readText()), expectedMetadata, `${label}: copy control writes the complete metadata`);
          await page.keyboard.press('Escape');
          assert.equal(await metadata.count(), 0, `${label}: Escape closes metadata popover`);
          assert.equal(await metadataToggle.evaluate(element => document.activeElement === element), true, `${label}: Escape restores metadata trigger focus`);
          const scrollRegion = page.getByRole('region', { name: locale === 'en' ? 'Request records (scroll horizontally)' : '请求记录（可横向滚动）' });
          await scrollRegion.focus();
          assert.equal(await scrollRegion.evaluate(element => document.activeElement === element), true, `${label}: horizontal table has an accessible keyboard entry`);
          assert.equal(await scrollRegion.evaluate(element => getComputedStyle(element).outlineStyle !== 'none'), true, `${label}: table focus visible`);
          if (width <= 390) assert.ok(await metadataToggle.evaluate(element => element.getBoundingClientRect().height >= 44), `${label}: metadata touch target`);
          if (theme === 'light') assert.equal(await action.evaluate(element => getComputedStyle(element).color), 'rgb(8, 121, 110)', `${label}: accessible light-theme link token`);
          const requestIdentifiers = page.locator('[data-fixture-request="recorded"] .request-diagnostics').getByRole('button', { name: locale === 'en' ? 'Record identifiers · Details' : '记录标识 · 详细信息', exact: true });
          assert.equal(await page.locator('.request-metadata-surface').count(), 0, `${label}: request identifiers are supplemental, not permanent rows`);
          // Measure the trigger while it is an interactive accessibility-tree
          // member. The open trapFocus popover intentionally hides background
          // controls from role queries until it closes.
          if (width <= 390) assert.ok(await requestIdentifiers.evaluate(element => element.getBoundingClientRect().height >= 44), `${label}: request metadata touch target`);
          await requestIdentifiers.focus();
          await page.keyboard.press('Enter');
          const requestMetadata = page.locator('.request-metadata-surface');
          await requestMetadata.waitFor({ state: 'visible' });
          const requestId = '5a3bc0cc-8d47-4cee-9b5e-2581f8d99d13';
          assert.equal(await requestMetadata.locator('code').textContent(), requestId, `${label}: complete request ID remains available`);
          const requestCopy = requestMetadata.getByRole('button', { name: locale === 'en' ? 'Copy Request ID' : '复制 请求 ID', exact: true });
          if (width <= 390) {
            assert.ok(await requestCopy.evaluate(element => element.getBoundingClientRect().height >= 44), `${label}: request ID copy target`);
          }
          await requestCopy.focus();
          await page.keyboard.press('Enter');
          assert.equal(await page.evaluate(() => navigator.clipboard.readText()), requestId, `${label}: keyboard copies the complete request ID`);
          await page.keyboard.press('Escape');
          await requestMetadata.waitFor({ state: 'detached' });
          // Fluent/Tabster restores focus asynchronously after the surface is
          // removed. DOM detachment alone does not mean restoration has run.
          await page.waitForFunction(element => document.activeElement === element, await requestIdentifiers.elementHandle(), { timeout: 5_000 });
          assert.equal(await requestIdentifiers.evaluate(element => document.activeElement === element), true, `${label}: request metadata restores keyboard focus`);
        }
        if (width === 390 || width === 1440) await page.screenshot({ path: `${artifacts}/${route.name}-${locale}-${theme}-${width}.png`, fullPage: true });
      }
      assert.deepEqual(errors, [], `${route.name} ${locale}: runtime errors`);
      await page.close();
    }
  } finally { await browser.close(); await server.close(); }
});
