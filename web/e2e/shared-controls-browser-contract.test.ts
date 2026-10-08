import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium, type Locator } from 'playwright';
import { createIsolatedFixtureServer } from './support/isolated-vite-server.js';
import { fixtureAssets } from './support/fixture-assets.js';

const cssFiles = ['styles.css', 'operator/operator.css', 'operator/managementSurfaces.css'];
const controls = ['refresh', 'save', 'compact', 'icon', 'disabled'];

function actionHeight(fluentHeight: number, viewportWidth: number) {
  return viewportWidth <= 768 ? Math.max(44, fluentHeight) : fluentHeight;
}

function targetsLegacyControl(selector: string) {
  return /(?:^|[\s>+~,(])(?:button|input|select|textarea)(?![\w-])|\.(?:button|secondary|compact-button)(?![\w-])/.test(selector);
}

function selectors(value: string) {
  const result: string[] = [];
  let depth = 0;
  let start = 0;
  for (let index = 0; index < value.length; index += 1) {
    if (value[index] === '(') depth += 1;
    if (value[index] === ')') depth -= 1;
    if (value[index] === ',' && depth === 0) {
      result.push(value.slice(start, index));
      start = index + 1;
    }
  }
  result.push(value.slice(start));
  return result;
}

async function geometry(control: Locator) {
  return control.evaluate(element => {
    const style = getComputedStyle(element);
    const bounds = element.getBoundingClientRect();
    const walker = document.createTreeWalker(element, NodeFilter.SHOW_TEXT);
    const textRects: { x: number; y: number; width: number; height: number }[] = [];
    while (walker.nextNode()) {
      if (!walker.currentNode.textContent?.trim()) continue;
      const range = document.createRange();
      range.selectNodeContents(walker.currentNode);
      textRects.push(...Array.from(range.getClientRects(), rect => ({ x: rect.x, y: rect.y, width: rect.width, height: rect.height })));
    }
    return {
      x: bounds.x, y: bounds.y, height: bounds.height, width: bounds.width, text: element.textContent,
      textRects, textLineCount: new Set(textRects.map(rect => rect.y)).size,
      font: style.font, lineHeight: style.lineHeight,
      paddingBlock: [style.paddingTop, style.paddingBottom],
      paddingInline: [style.paddingLeft, style.paddingRight],
      color: style.color, background: style.backgroundColor,
      border: [style.borderTopWidth, style.borderTopColor],
    };
  });
}

test('shared legacy control selectors cannot size or repaint Fluent slots', async () => {
  for (const selector of ['.tenant-dialog-input', '.button-row', '.selection-chip', '.fui-Dropdown__button']) {
    assert.equal(targetsLegacyControl(selector), false, `${selector}: a class name is not a native control selector`);
  }
  for (const selector of ['.tenant-dialog-input input', '.row-actions > button', ':where(button,.button,input,select,textarea)', '.form-panel :is(button,input)', '.secondary', '.toolbar .compact-button']) {
    assert.equal(targetsLegacyControl(selector), true, `${selector}: native control selectors remain covered`);
  }
  for (const file of cssFiles) {
    const css = (await readFile(new URL(`../src/${file}`, import.meta.url), 'utf8')).replace(/\/\*[\s\S]*?\*\//g, '');
    for (const match of css.matchAll(/([^{}]+)\{([^{}]*)\}/g)) {
      if (!/(?:^|;)\s*(?:font(?:-size|-weight|-family)?|line-height|padding(?:-block|-inline)?|background|border(?:-color)?|color)\s*:/m.test(match[2])) continue;
      for (const selector of selectors(match[1].trim())) {
        if (!targetsLegacyControl(selector)) continue;
        assert.ok(selector.includes('not([class*="fui-"])') || selector.includes('not(.fui-Button)'), `${file}: legacy control rule must exclude Fluent: ${selector.trim()}`);
      }
    }
  }
  const styles = await readFile(new URL('../src/styles.css', import.meta.url), 'utf8');
  assert.match(styles, /:where\(button,\.button,input,select,textarea\):not\(\[class\*="fui-"\]\)/);
  const operator = await readFile(new URL('../src/operator/operator.css', import.meta.url), 'utf8');
  assert.doesNotMatch(operator, /\.row-actions button,\s*\.account-meta \.row-actions button/);
  assert.match(operator, /\.row-actions,\s*\.row-actions button\s*\{\s*width:\s*100%/);
});

test('shared page action contexts retain Fluent geometry, labels, keyboard focus and theme', async context => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required for shared controls');
    context.skip('Chromium required'); return;
  }
  const server = await createIsolatedFixtureServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, plugins: [fixtureAssets()], logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const origin = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  const artifacts = fileURLToPath(new URL('../e2e-artifacts/ui-system/shared-controls/', import.meta.url));
  await mkdir(artifacts, { recursive: true });
  const measurements: unknown[] = [];
  let mediumHeight = 0;
  try {
    for (const locale of ['en', 'zh-CN']) for (const theme of ['light', 'dark']) for (const width of [320, 390, 1440]) {
      const label = `${locale}-${theme}-${width}`;
      const page = await browser.newPage({ viewport: { width, height: 900 } });
      const baseline = await browser.newPage({ viewport: { width, height: 900 } });
      const errors: string[] = [];
      for (const target of [page, baseline]) {
        target.on('pageerror', error => errors.push(error.message));
        await target.route('**/*', async route => {
          assert.equal(new URL(route.request().url()).origin, origin, 'fixture makes no external requests');
          await route.continue();
        });
      }
      const query = `locale=${locale}&theme=${theme}`;
      await baseline.goto(`${origin}/e2e/fixtures/shared-controls.html?${query}&baseline=1`);
      await page.goto(`${origin}/e2e/fixtures/shared-controls.html?${query}`);
      await page.locator('[data-controls="fields"] .fui-Switch').waitFor();
      await baseline.locator('[data-controls="fields"] .fui-Switch').waitFor();
      await page.screenshot({ path: `${artifacts}/${label}.png`, fullPage: true });
      const references = Object.fromEntries(await Promise.all(controls.map(async control => [control, await geometry(baseline.locator(`[data-controls="reference"] [data-control="${control}"]`))])));
      mediumHeight = references.refresh.height;
      assert.ok(references.compact.height < references.refresh.height, `${label}: clean Fluent baseline without the application touch policy retains intrinsic small/medium sizes`);
      assert.ok(references.icon.width < references.refresh.width, `${label}: clean Fluent baseline retains intrinsic icon sizing`);
      const surfaces = page.locator('[data-controls]').filter({ has: page.locator('[data-control="refresh"]') });
      for (const surface of await surfaces.all()) {
        const name = await surface.getAttribute('data-controls');
        const dimensions: Record<string, Awaited<ReturnType<typeof geometry>>> = {};
        for (const control of controls) {
          const measured = await geometry(surface.locator(`[data-control="${control}"]`));
          dimensions[control] = measured;
          const expected = references[control];
          assert.equal(measured.height, actionHeight(expected.height, width), `${label}/${name}/${control}: Fluent height with the narrow touch minimum`);
          if (width <= 768) assert.ok(measured.width >= 44 && measured.height >= 44, `${label}/${name}/${control}: at least 44x44px without changing Fluent typography or padding`);
          assert.equal(measured.font, expected.font, `${label}/${name}/${control}: Fluent typography`);
          assert.equal(measured.lineHeight, expected.lineHeight, `${label}/${name}/${control}: Fluent line height`);
          assert.deepEqual(measured.paddingBlock, expected.paddingBlock, `${label}/${name}/${control}: Fluent block padding`);
          assert.deepEqual(measured.paddingInline, expected.paddingInline, `${label}/${name}/${control}: Fluent inline padding`);
          assert.equal(measured.color, expected.color, `${label}/${name}/${control}: theme foreground`);
          assert.equal(measured.background, expected.background, `${label}/${name}/${control}: theme background`);
          assert.deepEqual(measured.border, expected.border, `${label}/${name}/${control}: theme border`);
          assert.equal(measured.text, expected.text, `${label}/${name}/${control}: complete label`);
          measurements.push({ label, surface: name, control, ...measured });
        }
        if (width > 768) {
          assert.ok(dimensions.compact.height < dimensions.refresh.height, `${label}/${name}: desktop preserves the explicit Fluent small/medium height distinction`);
        } else {
          assert.equal(dimensions.refresh.height, 44, `${label}/${name}: narrow single-line medium actions use the shared 44px target`);
          assert.equal(dimensions.compact.height, dimensions.refresh.height, `${label}/${name}: narrow small actions share the touch height while retaining their verified Fluent font and padding`);
          assert.equal(dimensions.icon.height, dimensions.refresh.height, `${label}/${name}: narrow icon actions share the touch height`);
        }
        const action = surface.locator('[data-control="refresh"]');
        await action.focus();
        await page.keyboard.press('Tab');
        await page.keyboard.press('Shift+Tab');
        assert.equal(await action.evaluate(element => document.activeElement === element), true, `${label}/${name}: keyboard target`);
        assert.equal(await action.evaluate(element => {
          const styles = [getComputedStyle(element), getComputedStyle(element, '::before'), getComputedStyle(element, '::after')];
          return styles.some(style => (style.outlineStyle !== 'none' && parseFloat(style.outlineWidth) > 0)
            || (style.content !== 'none' && style.opacity !== '0' && style.borderTopStyle === 'solid' && parseFloat(style.borderTopWidth) >= 2 && style.borderTopColor !== 'rgba(0, 0, 0, 0)'));
        }), true, `${label}/${name}: visible Fluent keyboard focus`);
        await page.keyboard.press('Enter');
        assert.equal(await surface.locator('output').textContent(), '1', `${label}/${name}: keyboard activates the original action`);
      }
      for (const selector of ['.fui-Input__input', '.fui-Select__select', '.fui-Textarea__textarea', '.fui-Combobox__input', '.fui-Dropdown__button', '.fui-Checkbox__input', '.fui-Switch__input']) {
        const measured = await geometry(page.locator(`[data-controls="fields"] ${selector}`));
        const expected = await geometry(baseline.locator(`[data-controls="fields"] ${selector}`));
        assert.equal(measured.height, expected.height, `${label}/${selector}: Fluent field geometry`);
        assert.equal(measured.font, expected.font, `${label}/${selector}: Fluent field text`);
        assert.deepEqual(measured.paddingBlock, expected.paddingBlock, `${label}/${selector}: field block padding`);
        assert.deepEqual(measured.paddingInline, expected.paddingInline, `${label}/${selector}: field inline padding`);
      }
      const long = page.locator('[data-controls="long-label"] [data-control="long"]');
      assert.equal(await long.evaluate(element => element.scrollWidth <= element.clientWidth), true, `${label}: long action text fits its own control`);
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true, `${label}: no document overflow`);
      if (width <= 390) {
        assert.equal(await long.evaluate(element => getComputedStyle(element).whiteSpace), 'normal', `${label}: long action may wrap`);
        assert.ok((await geometry(long)).height > actionHeight(references.refresh.height, width), `${label}: wrapping grows naturally rather than clipping`);
      }
      const native = page.locator('[data-controls="native"] button').first();
      assert.ok((await geometry(native)).height >= actionHeight(references.refresh.height, width), `${label}: native actions retain a usable baseline`);
      const table = page.locator('[data-controls="table-actions"] table');
      const headerCells = await table.locator('thead th').all();
      const headerBounds = await Promise.all(headerCells.map(cell => cell.boundingBox()));
      const tableActions = await table.locator('.row-actions button').all();
      const tableMeasurements = await Promise.all(tableActions.map(action => geometry(action)));
      measurements.push(...tableMeasurements.map(measured => ({ label, surface: 'table-actions', ...measured })));
      await writeFile(`${artifacts}/measurements.json`, JSON.stringify(measurements, null, 2));
      for (const row of await table.locator('tbody tr').all()) {
        const cells = await row.locator('td').all();
        assert.equal(cells.length, headerBounds.length, `${label}: table rows retain their semantic columns`);
        for (const [index, cell] of cells.entries()) {
          const bounds = await cell.boundingBox();
          const heading = headerBounds[index];
          assert.ok(bounds && heading);
          assert.ok(Math.abs(bounds.x - heading.x) <= 1 && Math.abs(bounds.width - heading.width) <= 1, `${label}: header and rows share tracks regardless of action count`);
        }
      }
      for (const [index, action] of tableActions.entries()) {
        const measured = tableMeasurements[index];
        assert.equal(measured.height, (await geometry(native)).height, `${label}/${measured.text}: table actions retain the native single-line baseline`);
        assert.equal(measured.textLineCount, 1, `${label}/${measured.text}: intrinsic table width preserves short action words`);
        assert.equal(await action.evaluate(element => element.scrollWidth <= element.clientWidth), true, `${label}: table action labels remain contained`);
      }
      const credentialActions = page.locator('[data-controls="combobox-actions"]');
      const credentialInput = credentialActions.getByRole('combobox');
      const loadMore = credentialActions.locator('[data-control="load-more"]');
      await credentialInput.click();
      await credentialActions.getByRole('option', { name: 'Example credential' }).waitFor();
      await page.keyboard.press('Escape');
      const inputBounds = await credentialInput.boundingBox();
      const actionBounds = await loadMore.boundingBox();
      assert.ok(inputBounds && actionBounds);
      assert.ok(inputBounds.x + inputBounds.width <= actionBounds.x || inputBounds.y + inputBounds.height <= actionBounds.y, `${label}: credential input does not overlap its paging action`);
      await loadMore.click();
      assert.equal(await credentialActions.locator('output').textContent(), '1', `${label}: credential paging receives an ordinary pointer click`);
      const credentialRows = page.locator('[data-controls="credential-list"] .credential-compact-row');
      const columns: Array<{ identity: number; actions: number; identityWidth: number; actionsWidth: number }> = [];
      for (const row of await credentialRows.all()) {
        const identity = row.locator('.managed-resource-header');
        const name = identity.locator('b');
        assert.equal(await name.isVisible(), true, `${label}: credential name remains visible beside full action labels`);
        const nameBounds = await name.boundingBox();
        const identityBounds = await identity.boundingBox();
        const actionsBounds = await row.locator('.credential-row-actions').boundingBox();
        assert.ok(nameBounds && identityBounds && actionsBounds);
        assert.ok(nameBounds.width > 0 && nameBounds.height > 0, `${label}: credential identity never collapses to a zero rectangle`);
        assert.equal(await row.evaluate(element => element.scrollWidth <= element.clientWidth), true, `${label}: credential actions remain inside their row`);
        columns.push({ identity: identityBounds.x, actions: actionsBounds.x, identityWidth: identityBounds.width, actionsWidth: actionsBounds.width });
      }
      assert.deepEqual(columns[0], columns[1], `${label}: both credential structures share tracks regardless of action count`);
      if (width > 850) {
        const credentialList = page.locator('[data-controls="credential-list"]');
        await credentialList.evaluate(element => { (element as HTMLElement).style.maxWidth = '480px'; });
        for (const row of await credentialRows.all()) {
          const name = row.locator('.managed-resource-header b');
          assert.equal(await name.isVisible(), true, `${label}: narrow desktop card keeps the credential name visible`);
          const nameBounds = await name.boundingBox();
          const headerBounds = await row.locator('.managed-resource-header').boundingBox();
          const actionsBounds = await row.locator('.credential-row-actions').boundingBox();
          assert.ok(nameBounds && headerBounds && actionsBounds);
          assert.ok(nameBounds.width > 0 && nameBounds.height > 0, `${label}: narrow desktop card reserves a nonzero identity rectangle`);
          assert.ok(actionsBounds.y >= headerBounds.y + headerBounds.height, `${label}: narrow card stacks actions based on container width, not viewport`);
          assert.equal(await row.evaluate(element => element.scrollWidth <= element.clientWidth), true, `${label}: wrapped card actions remain contained`);
          for (const action of await row.locator('.credential-row-actions button').all()) await action.click();
        }
        await page.screenshot({ path: `${artifacts}/${label}-credential-card.png`, fullPage: true });
        await credentialList.evaluate(element => { (element as HTMLElement).style.removeProperty('max-width'); });
      }
      const selected = await geometry(page.locator('[data-control="selected"]'));
      assert.equal(selected.height, actionHeight(references.refresh.height, width), `${label}: selected state does not change button geometry`);
      assert.notEqual(selected.background, references.refresh.background, `${label}: selected state remains visually distinct in the active theme`);
      if (width <= 768) {
        for (const action of await page.locator('.fui-Button, button:not([class*="fui-"])').all()) {
          const measured = await geometry(action);
          assert.ok(measured.width >= 44 && measured.height >= 44, `${label}/${measured.text}: shared narrow action target is at least 44x44px`);
          measurements.push({ label, surface: 'narrow-touch-target', ...measured });
        }
      }
      await page.screenshot({ path: `${artifacts}/${label}.png`, fullPage: true });
      assert.deepEqual(errors, [], label);
      await page.close(); await baseline.close();
    }
    const pages = [
      { name: 'providers', fixture: 'form-journey.html?workflows', ready: '.provider-directory-actions .fui-Button', controls: '.provider-directory-actions .fui-Button', native: false },
      { name: 'routes', fixture: 'form-journey.html?workflows&view=routes', ready: '.model-route-list .row-actions button', controls: '.model-route-list .row-actions button', native: true },
      { name: 'usage', fixture: 'usage-analysis.html', ready: '.usage-heading .fui-Button', controls: '.usage-heading .fui-Button, .usage-presets .fui-Button, .usage-tabs .fui-Button', native: false },
    ];
    for (const route of pages) for (const theme of ['light', 'dark']) for (const width of [390, 1440]) {
      const page = await browser.newPage({ viewport: { width, height: 900 } });
      const errors: string[] = []; page.on('pageerror', error => errors.push(error.message));
      await page.addInitScript(() => { localStorage.setItem('mtc-locale', 'en'); });
      await page.route('**/*', async request => {
        assert.equal(new URL(request.request().url()).origin, origin, 'real page fixture stays local');
        await request.continue();
      });
      await page.goto(`${origin}/e2e/fixtures/${route.fixture}`);
      await page.evaluate(value => { document.documentElement.dataset.theme = value; }, theme);
      await page.locator(route.ready).first().waitFor();
      const actions = page.locator(route.controls);
      assert.ok(await actions.count() > 0, `${route.name}: real page actions are present`);
      const actionElements = await actions.all();
      const actionMeasurements = await Promise.all(actionElements.map(action => geometry(action)));
      measurements.push(...actionMeasurements.map(measured => ({ page: route.name, theme, viewportWidth: width, ...measured })));
      await writeFile(`${artifacts}/measurements.json`, JSON.stringify(measurements, null, 2));
      await page.screenshot({ path: `${artifacts}/page-${route.name}-${theme}-${width}.png`, fullPage: true });
      const expectedHeight = route.native ? (await geometry(actions.first())).height : actionHeight(mediumHeight, width);
      assert.ok(expectedHeight >= actionHeight(mediumHeight, width), `${route.name}: usable action baseline`);
      for (const [index, action] of actionElements.entries()) {
        const measured = actionMeasurements[index];
        assert.equal(measured.height, expectedHeight, `${route.name}/${theme}/${width}/${measured.text}: same default action height`);
        if (route.native) assert.equal(measured.textLineCount, 1, `${route.name}/${theme}/${width}/${measured.text}: short route actions do not wrap into character columns`);
        if (width <= 768) assert.ok(measured.width >= 44 && measured.height >= 44, `${route.name}/${theme}/${width}: shared narrow target is at least 44x44px`);
        assert.ok(measured.text?.trim(), `${route.name}: visible action label`);
        assert.equal(await action.evaluate(element => element.scrollWidth <= element.clientWidth), true, `${route.name}: action text contained`);
      }
      assert.deepEqual(errors, [], route.name);
      await page.close();
    }
    await writeFile(`${artifacts}/measurements.json`, JSON.stringify(measurements, null, 2));
  } finally { await browser.close(); await server.close(); }
});
