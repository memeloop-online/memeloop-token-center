import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import { mkdir } from 'node:fs/promises';
import test from 'node:test';
import { chromium, type Locator } from 'playwright';
import { createIsolatedFixtureServer as createServer } from './support/isolated-vite-server.js';

declare global { interface Window { routeListWrites: number; routeTooltipTrace: { begin: (trigger: Element, label: { locale: string; width: number; theme: string; activation: string }) => void; read: () => unknown; stop: () => void } } }

for (const activation of ['original', 'anchor'] as const) test(`route list exposes group-only candidate scope and readable models without changing routing${activation === 'original' ? '' : ' (keyboard anchor control)'}`, { timeout: 45_000 }, async () => {
  const primitives = fileURLToPath(new URL('../src/design-system/primitives.tsx', import.meta.url));
  let instrumented = false;
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', plugins: [{
    name: 'route-tooltip-readonly-trace', enforce: 'pre',
    transform(source, id) {
      if (id.split('?')[0] !== primitives) return;
      const original = 'onVisibleChange={(_, data) => setVisible(data.visible)}';
      assert.equal(source.split(original).length - 1, 1, 'instrument exactly the existing DetailTooltip visibility callback');
      instrumented = true;
      return source.replace(original, `onVisibleChange={(event, data) => { document.dispatchEvent(new CustomEvent('route-tooltip-visible', { detail: { eventType: event?.type, visible: data.visible, key: data.documentKeyboardEvent?.key ?? (event && 'key' in event ? event.key : undefined), target: event?.target } })); setVisible(data.visible); }}`);
    },
  }], server: { host: '127.0.0.1', port: 0 } });
  await server.listen(); const address = server.httpServer!.address(); assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  const artifacts = fileURLToPath(new URL('../e2e-artifacts/route-list-scope/', import.meta.url));
  await mkdir(artifacts, { recursive: true });
  try {
    const page = await browser.newPage({ hasTouch: true }); const errors: string[] = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.addInitScript(`(() => {
      let trigger, label, started = 0, total = 0;
      let first = [], recent = [], callbacks = { show: 0, hide: 0, targetShow: 0, targetHide: 0 };
      const relation = element => element === trigger ? 'trigger' : element instanceof Element && element.closest('[role="tooltip"]') ? 'tooltip' : element instanceof Element && element.matches('[data-tooltip-keyboard-anchor]') ? 'anchor' : 'other';
      const box = element => { const rect = element.getBoundingClientRect(); return { x: rect.x, y: rect.y, width: rect.width, height: rect.height }; };
      const surfaces = () => (trigger.getAttribute('aria-describedby') || '').split(/\\s+/).filter(Boolean).slice(0, 4).map(id => {
        const element = document.getElementById(id);
        if (!element) return { id: id.slice(0, 80), missing: true };
        const style = getComputedStyle(element);
        return { id: id.slice(0, 80), tooltip: element.getAttribute('role') === 'tooltip', display: style.display, visibility: style.visibility, opacity: style.opacity, position: style.position, hidden: element.hidden, referenceHidden: element.hasAttribute('data-popper-reference-hidden'), escaped: element.hasAttribute('data-popper-escaped'), bounds: box(element) };
      });
      const record = (kind, detail = {}, sampleSurface = false) => {
        if (!trigger) return;
        const entry = { sequence: ++total, ms: Math.round(performance.now() - started), kind, ...detail, focus: document.activeElement === trigger, active: relation(document.activeElement), documentVisibility: document.visibilityState, ...(sampleSurface ? { hover: trigger.matches(':hover'), triggerBounds: box(trigger), surfaces: surfaces(), scroll: [scrollX, scrollY] } : {}) };
        if (first.length < 32) first.push(entry); else { recent.push(entry); if (recent.length > 32) recent.shift(); }
      };
      const listen = event => {
        if (!trigger) return;
        if (event.type === 'route-tooltip-visible') {
          const detail = event.detail;
          const target = relation(detail.target);
          callbacks[detail.visible ? 'show' : 'hide']++;
          if (target === 'trigger') callbacks[detail.visible ? 'targetShow' : 'targetHide']++;
          record('visible-request', { eventType: detail.eventType, visible: detail.visible, key: ['Tab', 'Escape'].includes(detail.key) ? detail.key : undefined, target });
          return;
        }
        if (event.type === 'keydown' && !['Tab', 'Escape'].includes(event.key)) return;
        record(event.type, { target: relation(event.target), related: relation(event.relatedTarget), trusted: event.isTrusted, key: ['Tab', 'Escape'].includes(event.key) ? event.key : undefined, shift: Boolean(event.shiftKey), programmatic: typeof event.detail?.isFocusedProgrammatically === 'boolean' ? event.detail.isFocusedProgrammatically : undefined });
      };
      for (const type of ['route-tooltip-visible', 'focusin', 'focusout', 'keyborg:focusin', 'keydown', 'click', 'pointerdown', 'pointerup', 'pointerenter', 'pointerleave', 'scroll', 'visibilitychange']) document.addEventListener(type, listen, true);
      const observer = new MutationObserver(records => {
        if (!trigger) return;
        const ids = (trigger.getAttribute('aria-describedby') || '').split(/\\s+/);
        const relevant = records.some(record => record.target === trigger || record.target instanceof Element && (record.target.matches('[role="tooltip"]') || ids.includes(record.target.id)) || [...record.addedNodes, ...record.removedNodes].some(node => node instanceof Element && (node.matches('[role="tooltip"]') || node.querySelector('[role="tooltip"]'))));
        if (relevant) record('tooltip-mutation', { types: [...new Set(records.map(item => item.type))], attributes: [...new Set(records.map(item => item.attributeName).filter(Boolean))].slice(0, 8) }, true);
      });
      observer.observe(document, { subtree: true, childList: true, attributes: true, attributeFilter: ['aria-describedby', 'style', 'class', 'hidden', 'data-popper-reference-hidden', 'data-popper-escaped', 'data-popper-placement'] });
      window.routeTooltipTrace = {
        begin: (element, nextLabel) => { trigger = element; label = nextLabel; started = performance.now(); total = 0; first = []; recent = []; callbacks = { show: 0, hide: 0, targetShow: 0, targetHide: 0 }; record('begin', {}, true); },
        read: () => { record('snapshot', {}, true); return { label, total, callbacks, events: [...first, ...recent] }; },
        stop: () => { trigger = undefined; }
      };
    })()`);
    const describedTooltip = async (trigger: Locator, expectedText: string | RegExp) => {
      const element = await trigger.elementHandle();
      assert.ok(element);
      const idHandle = await page.waitForFunction(currentTrigger => {
        const ids = (currentTrigger.getAttribute('aria-describedby') ?? '').split(/\s+/).filter(Boolean);
        return ids.length ? ids : null;
      }, element);
      const ids = await idHandle.jsonValue() as string[];
      const tooltip = page.locator(ids.map(id => `[id=${JSON.stringify(id)}]`).join(', ')).and(page.getByRole('tooltip', { includeHidden: true }));
      assert.equal(await tooltip.count(), 1, 'trigger accessible description must resolve to a single tooltip surface');
      await tooltip.waitFor({ state: 'visible' });
      const tooltipId = await tooltip.getAttribute('id');
      assert.ok(tooltipId, 'visible tooltip must expose an id for its accessible description');
      const describedBy = (await trigger.getAttribute('aria-describedby') ?? '').split(/\s+/).filter(Boolean);
      assert.ok(describedBy.includes(tooltipId), 'visible tooltip must be referenced by its trigger accessible description');
      const text = await tooltip.innerText();
      if (typeof expectedText === 'string') assert.ok(text.includes(expectedText));
      else assert.match(text, expectedText);
      return tooltip;
    };
    for (const locale of ['zh-CN', 'en']) {
      await page.addInitScript(value => localStorage.setItem('mtc-locale', value), locale);
      await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/route-list-scope.html${activation === 'anchor' ? '?keyboard-anchor' : ''}`);
      assert.equal(instrumented, true, 'the exact primitives module must pass through diagnostic instrumentation');
      const rows = page.locator('.model-route-list tbody tr');
      const group = rows.filter({ hasText: 'Kimi group-only' });
      await group.getByText('Kimi 模型组', { exact: true }).waitFor();
      const range = group.getByRole('button', { name: locale === 'en' ? '2 candidate accounts' : '2 个候选账号', exact: true });
      assert.equal(await rows.filter({ hasText: 'Kimi mixed' }).getByRole('button', { name: locale === 'en' ? '1 candidate accounts' : '1 个候选账号', exact: true }).count(), 1);
      assert.equal(await rows.filter({ hasText: 'Kimi empty' }).getByText(locale === 'en' ? '0 candidate accounts' : '0 个候选账号', { exact: true }).count(), 1);
      assert.equal(await rows.filter({ hasText: 'Kimi unknown' }).getByText(locale === 'en' ? 'Account range unknown' : '账号范围未知', { exact: true }).count(), 1);
      assert.match(await rows.filter({ hasText: 'Kimi direct' }).locator('.route-list-source-name').innerText(), /Kimi personal account/);
      const mixedRange = rows.filter({ hasText: 'Kimi mixed' }).getByRole('button', { name: locale === 'en' ? '1 candidate accounts' : '1 个候选账号', exact: true });
      await mixedRange.click();
      const mixedTip = await describedTooltip(mixedRange, /团队排除组/);
      assert.match(await mixedTip.innerText(), /团队排除组/);
      assert.doesNotMatch(await mixedTip.innerText(), /Kimi team account/, 'the list must not re-expand excluded members beyond the server candidate set');
      await page.keyboard.press('Escape');
      await mixedTip.waitFor({ state: 'hidden' });
      for (const [width, theme] of [[390, 'light'], [1440, 'dark']] as const) {
        await page.setViewportSize({ width, height: 900 }); await page.evaluate(theme => { document.documentElement.dataset.theme = theme; }, theme);
        await range.evaluate((element, label) => window.routeTooltipTrace.begin(element, label), { locale, width, theme, activation });
        let outcome = 'failed';
        try {
          if (activation === 'original') {
            await range.focus(); await range.press('Shift+Tab'); await page.keyboard.press('Tab');
          } else {
            const anchor = page.locator('[data-tooltip-keyboard-anchor]');
            await anchor.click();
            assert.equal(await anchor.evaluate(element => element === document.activeElement), true);
            await page.mouse.move(-10, -10);
            await mixedTip.waitFor({ state: 'hidden' });
            for (let index = 0; index < 64 && !await range.evaluate(element => element === document.activeElement); index++) await page.keyboard.press('Tab');
            await mixedTip.waitFor({ state: 'hidden' });
          }
          assert.equal(await range.evaluate(el => el === document.activeElement), true);
          const tip = await describedTooltip(range, /Kimi personal account/);
          assert.match(await tip.innerText(), /Kimi personal account/); assert.match(await tip.innerText(), /Kimi team account/);
          await page.keyboard.press('Escape'); await tip.waitFor({ state: 'hidden' });
          await range.tap(); await describedTooltip(range, /Kimi personal account/);
          const bounds = await tip.boundingBox(); assert.ok(bounds && bounds.x >= -1 && bounds.x + bounds.width <= width + 1);
          const model = group.locator('.route-model-name').first();
          const style = await model.evaluate(el => { const cs = getComputedStyle(el); const probe = document.createElement('span'); probe.style.color = 'var(--colorNeutralForeground1)'; el.append(probe); const expected = getComputedStyle(probe).color; probe.remove(); return { color: cs.color, expected, weight: Number(cs.fontWeight), text: el.textContent }; });
          assert.equal(style.color, style.expected); assert.ok(style.weight >= 600); assert.equal(style.text, 'Kimi group-only');
          assert.equal(await group.locator('.route-model-name').last().textContent(), 'kimi-k2.5');
          assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false);
          await page.screenshot({ path: `${artifacts}/route-list-${locale}-${width}-${theme}${activation === 'original' ? '' : '-anchor'}.png`, fullPage: true });
          await page.keyboard.press('Escape');
          await tip.waitFor({ state: 'hidden' });
          outcome = 'passed';
        } finally {
          try {
            console.log('Route tooltip event trace', JSON.stringify({ outcome, trace: await page.evaluate(() => window.routeTooltipTrace.read()) }));
          } catch {
            console.log('Route tooltip event trace unavailable', JSON.stringify({ locale, width, theme, activation, outcome }));
          }
          await page.evaluate(() => window.routeTooltipTrace.stop()).catch(() => undefined);
        }
      }
      assert.equal(await page.evaluate(() => window.routeListWrites), 0);
    }
    assert.deepEqual(errors, []);
  } finally { await browser.close(); await server.close(); }
});
