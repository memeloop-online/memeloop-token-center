import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { readdir, readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

import { chromium } from 'playwright';
import { createServer } from 'vite';

const appShellStylesPath = fileURLToPath(new URL('../src/app-shell.css', import.meta.url));
const sharedStylesPath = fileURLToPath(new URL('../src/styles.css', import.meta.url));
const webRoot = fileURLToPath(new URL('..', import.meta.url));

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

test('1024px compact sidebar keeps every visually hidden navigation label accessible', { timeout: 30_000 }, async () => {
  const executablePath = await localChromiumExecutable();
  if (!executablePath) return test.skip('a local Chromium runtime is required for the 1024px sidebar assertion');
  const componentSource = await readFile(new URL('../src/app/AppShell.tsx', import.meta.url), 'utf8');
  assert.match(componentSource, /aria-label=\{item\.label\}/);
  assert.match(componentSource, /title=\{item\.label\}/);
  const browser = await chromium.launch({ executablePath, headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 1024, height: 720 } });
    await page.setContent(`<div class="product-app"><aside class="app-sidebar" aria-label="Operator"><div class="app-brand"><img alt=""><span><b>Token Center</b><small>Operator</small></span></div><nav class="app-navigation" aria-label="Operator"><section class="app-nav-section"><h2>Monitoring</h2><a class="app-nav-item" href="/operator?view=requests" aria-current="page" aria-label="Requests" title="Requests"><svg class="app-nav-icon" aria-hidden="true"></svg><span>Requests</span></a></section></nav><div class="app-sidebar-footer"><button type="button"><span>☀</span><b>Light theme</b></button></div></aside><div class="app-stage"></div></div>`);
    await page.addStyleTag({ path: appShellStylesPath });
    const link = page.getByRole('link', { name: 'Requests', exact: true });
    await link.focus();
    assert.equal(await link.evaluate((element) => document.activeElement === element), true);
    assert.equal(await link.getAttribute('title'), 'Requests');
    assert.equal(await link.getAttribute('aria-label'), 'Requests');
    const layout = await link.evaluate((element) => ({
      height: element.getBoundingClientRect().height,
      labelDisplay: getComputedStyle(element.querySelector<HTMLElement>('span')!).display,
      sidebarWidth: element.closest<HTMLElement>('.app-sidebar')!.getBoundingClientRect().width,
    }));
    assert.equal(layout.labelDisplay, 'none');
    assert.equal(layout.sidebarWidth, 72);
    assert.ok(layout.height >= 44);
  } finally { await browser.close(); }
});

test('shell and route content share the exact 899/900/901 responsive boundary without container overflow', { timeout: 30_000 }, async () => {
  const executablePath = await localChromiumExecutable();
  if (!executablePath) return test.skip('a local Chromium runtime is required for boundary overflow assertions');
  const browser = await chromium.launch({ executablePath, headless: true });
  try {
    const page = await browser.newPage();
    await page.setContent(`<div class="product-app"><aside class="app-sidebar"><div class="app-brand"><img alt=""><span><b>Token Center</b><small>Operator</small></span></div></aside><div class="app-stage"><header class="app-context-bar"><div class="app-breadcrumb"><strong>Requests</strong></div></header><main class="app-main-content"><header class="hero"><div><h1>Requests</h1><p>Review recent requests</p></div><form class="credential"><input aria-label="Credential"><button>Connect</button></form></header><section class="metrics"><article class="metric">1</article><article class="metric">2</article></section><section class="two-column"><article class="panel">A</article><article class="panel">B</article></section><section class="overflow-probe">A responsive route content container</section></main></div></div>`);
    await page.addStyleTag({ path: sharedStylesPath });
    await page.addStyleTag({ path: appShellStylesPath });
    await page.addStyleTag({ content: 'html, body { overflow-x: visible !important; } .overflow-probe { width: 100%; min-width: 0; }' });
    for (const width of [899, 900, 901]) {
      await page.setViewportSize({ width, height: 720 });
      const layout = await page.evaluate(() => {
        const sidebar = document.querySelector<HTMLElement>('.app-sidebar')!;
        const stage = document.querySelector<HTMLElement>('.app-stage')!;
        const containers = [document.documentElement, document.querySelector<HTMLElement>('.product-app')!, stage, document.querySelector<HTMLElement>('.app-main-content')!, document.querySelector<HTMLElement>('.overflow-probe')!];
        return {
          containers: containers.map((element) => ({ clientWidth: element.clientWidth, scrollWidth: element.scrollWidth })),
          sidebarPosition: getComputedStyle(sidebar).position,
          sidebarWidth: sidebar.getBoundingClientRect().width,
          stageWidth: stage.getBoundingClientRect().width,
          heroDirection: getComputedStyle(document.querySelector<HTMLElement>('.hero')!).flexDirection,
        };
      });
      for (const container of layout.containers) assert.ok(container.scrollWidth <= container.clientWidth, `${width}px container must not hide horizontal overflow`);
      if (width <= 900) {
        assert.equal(layout.sidebarPosition, 'fixed');
        assert.equal(layout.stageWidth, width);
        assert.equal(layout.heroDirection, 'column');
      } else {
        assert.equal(layout.sidebarPosition, 'sticky');
        assert.equal(layout.sidebarWidth, 72);
        assert.equal(layout.stageWidth, width - 72);
        assert.equal(layout.heroDirection, 'row');
      }
    }
  } finally { await browser.close(); }
});

test('portal and operator primitives remain readable from 320px mobile to 2560px widescreen', { timeout: 30_000 }, async () => {
  const executablePath = await localChromiumExecutable();
  if (!executablePath) return test.skip('a local Chromium runtime is required for the responsive matrix');
  const operatorStylesPath = fileURLToPath(new URL('../src/operator/operator.css', import.meta.url));
  const usageStylesPath = fileURLToPath(new URL('../src/operator/usage.css', import.meta.url));
  const browser = await chromium.launch({ executablePath, headless: true });
  try {
    const page = await browser.newPage();
    await page.setContent(`<div class="product-app"><aside class="app-sidebar"></aside><div class="app-stage"><header class="app-context-bar"><button class="app-mobile-menu">☰</button><div class="app-breadcrumb"><small>Identity and access</small><strong>Service credentials with a deliberately long route title</strong></div><div class="app-mobile-preferences"><button>EN</button><button>☀</button></div></header><main class="app-main-content"><header class="hero"><div><h1>Traffic monitoring</h1><p>Recent activity and current health</p></div><form class="credential"><input aria-label="Credential"><button>Connect</button></form></header><section class="metrics usage-metrics"><article class="metric"><span class="metric-label">Total requests</span><strong class="metric-value">123,456</strong></article><article class="metric"><span class="metric-label">Success rate</span><strong class="metric-value">99.95%</strong></article></section><article class="panel"><div class="panel-title traffic-heading"><div><h2>Recent requests</h2><span>Live updates</span></div><div class="traffic-heading-actions"><div class="session-live-state live">Live</div><div class="segmented"><button>Requests</button><button>Sessions</button></div></div></div><div class="table-scroll"><table><tbody><tr><td>2026-08-30 10:02:15</td><td>gpt-5.6-luna-with-a-long-model-name</td><td>successful</td><td>128,000</td><td>US$123.45</td></tr></tbody></table></div></article><section class="session-list"><article class="session-card"><div class="session-card-heading"><b>A deliberately long session title that must wrap safely</b><span><i class="status ok">active</i><i class="pill">foreground</i></span></div></article><article class="session-card"><div class="session-card-heading"><b>Second independent session</b><span><i class="status ok">active</i></span></div></article></section></main></div></div>`);
    await page.addStyleTag({ path: sharedStylesPath });
    await page.addStyleTag({ path: appShellStylesPath });
    await page.addStyleTag({ path: operatorStylesPath });
    await page.addStyleTag({ path: usageStylesPath });
    await page.addStyleTag({ content: 'html, body { overflow-x: visible !important; }' });
    for (const width of [320, 390, 768, 900, 1024, 1440, 1920, 2560]) {
      await page.setViewportSize({ width, height: 900 });
      const layout = await page.evaluate(() => {
        const root = document.documentElement;
        const main = document.querySelector<HTMLElement>('.app-main-content')!;
        const context = document.querySelector<HTMLElement>('.app-context-bar')!;
        const table = document.querySelector<HTMLElement>('.table-scroll')!;
        const sessionHeading = document.querySelector<HTMLElement>('.session-card-heading')!;
        return {
          root: [root.clientWidth, root.scrollWidth],
          main: [main.clientWidth, main.scrollWidth],
          context: [context.clientWidth, context.scrollWidth],
          table: [table.clientWidth, table.scrollWidth],
          sessionColumns: getComputedStyle(sessionHeading).gridTemplateColumns,
          sessionListColumns: getComputedStyle(document.querySelector<HTMLElement>('.session-list')!).gridTemplateColumns.split(' ').length,
          metricColumns: getComputedStyle(document.querySelector<HTMLElement>('.usage-metrics')!).gridTemplateColumns.split(' ').length,
        };
      });
      assert.ok(layout.root[1] <= layout.root[0], `${width}px document must not overflow`);
      assert.ok(layout.main[1] <= layout.main[0], `${width}px main content must not overflow`);
      assert.ok(layout.context[1] <= layout.context[0], `${width}px context bar must not overflow`);
      assert.ok(layout.table[1] >= layout.table[0], `${width}px wide tables must remain inside their scroll container`);
      if (width <= 480) assert.equal(layout.sessionColumns.split(' ').length, 1, `${width}px session heading must stack`);
      if (width <= 350) assert.equal(layout.metricColumns, 1, `${width}px usage metrics must use a readable single column`);
      if (width <= 1100) assert.equal(layout.sessionListColumns, 1, `${width}px session summaries must retain a readable single column`);
      if (width >= 1440) assert.ok(layout.sessionListColumns >= 2, `${width}px session summaries must use available widescreen space`);
    }
  } finally { await browser.close(); }
});

test('a rejected lazy route reloads its module graph and recovers instead of staying blank', { timeout: 30_000 }, async () => {
  const executablePath = await localChromiumExecutable();
  if (!executablePath) return test.skip('a local Chromium runtime is required for lazy rejection assertions');
  const server = await createServer({ root: webRoot, configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0, strictPort: false } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ executablePath, headless: true });
  try {
    const page = await browser.newPage();
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/route-error-boundary.html`);
    const alert = page.getByRole('alert');
    await alert.waitFor();
    await page.getByRole('heading', { name: 'This page cannot be displayed' }).waitFor();
    await page.getByRole('button', { name: 'Refresh page' }).waitFor();
    assert.ok((await alert.textContent())?.includes('A page module did not finish loading.'));
    assert.ok((await page.locator('body').innerText()).trim().length > 0);
    await page.getByRole('button', { name: 'Try again' }).click();
    await page.getByRole('heading', { name: 'Recovered route module' }).waitFor();
    assert.equal(await page.getByRole('alert').count(), 0);
  } finally {
    await browser.close();
    await server.close();
  }
});
