import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer } from './support/isolated-vite-server.js';
import { requestEgressCopy } from '../src/operator/requestEgressCopy.js';
import './support/request-egress-fixtures.js';

test('operator exit history keeps addresses, coverage, scope and failed reads distinct with bilingual narrow screenshots', { timeout: 90_000 }, async () => {
  const server = await createIsolatedFixtureServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const origin = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  const artifactRoot = fileURLToPath(new URL('../e2e-artifacts/request-diagnostics/', import.meta.url));
  await mkdir(artifactRoot, { recursive: true });
  try {
    for (const locale of ['zh-CN', 'en']) {
      const page = await browser.newPage({ viewport: { width: 390, height: 844 } });
      const copy = requestEgressCopy(locale);
      const errors: string[] = [];
      page.on('pageerror', error => errors.push(error.message));
      await page.route('**/*', route => new URL(route.request().url()).origin === origin ? route.continue() : route.abort());
      await page.addInitScript(value => localStorage.setItem('mtc-locale', value), locale);
      await page.goto(`${origin}/e2e/fixtures/request-egress.html`);
      await page.getByText('Current exit · socks5h://current.proxy.test:1080', { exact: true }).waitFor();
      const summary = page.getByRole('region', { name: copy.title, exact: true });
      await summary.locator('[data-egress-summary]').first().waitFor();
      assert.equal(await summary.locator('[data-egress-summary]').count(), 3);
      assert.match(await summary.innerText(), /old\.proxy\.test|later\.proxy\.test/);
      assert.doesNotMatch(await summary.innerText(), /current\.proxy\.test|credential-canary/);
      assert.ok((await summary.innerText()).includes(copy.coverage(2, 3)));
      assert.ok((await summary.innerText()).includes('0 B'));
      await summary.getByLabel(copy.groupFilter, { exact: true }).selectOption('group-a');
      assert.equal(await summary.locator('[data-egress-summary]').count(), 2);
      await page.screenshot({ path: `${artifactRoot}/egress-summary-${locale}-390.png`, fullPage: true });
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1), true);
      for (const mode of ['denied', 'failed', 'empty']) {
        await page.evaluate(value => { window.egressFixture.mode = value; }, mode);
        await summary.getByRole('button', { name: copy.refresh, exact: true }).click();
        await summary.getByText(mode === 'empty' ? copy.empty : mode === 'denied' ? copy.denied : copy.failed, { exact: true }).waitFor();
        assert.equal(await summary.locator('[data-egress-summary]').count(), 0);
        assert.doesNotMatch(await summary.innerText(), /credential-canary/);
        await page.screenshot({ path: `${artifactRoot}/egress-${mode}-${locale}-390.png`, fullPage: true });
      }
      await page.evaluate(() => { window.egressFixture.mode = 'slow'; });
      await summary.getByRole('button', { name: copy.refresh, exact: true }).click();
      await page.waitForFunction(() => Boolean(window.egressFixture.release));
      await summary.getByRole('group', { name: copy.loading, exact: true }).waitFor();
      await page.screenshot({ path: `${artifactRoot}/egress-loading-${locale}-390.png`, fullPage: true });
      await page.evaluate(() => { window.egressFixture.mode = 'ready'; window.egressFixture.switchTenant(); });
      await summary.getByText(copy.empty, { exact: true }).waitFor();
      await page.evaluate(() => window.egressFixture.release?.());
      assert.equal(await summary.locator('[data-egress-summary]').count(), 0);
      assert.ok(await page.evaluate(() => window.egressFixture.reads.every(query => new URLSearchParams(query).has('tenant_external_id'))));
      await page.goto(`${origin}/e2e/fixtures/request-lifecycle.html?egress=1&quiet-background-help`);
      const row = page.locator('tbody tr').filter({ has: page.getByText('model-a', { exact: true }) });
      await row.locator('.table-action').click();
      const drawer = page.getByRole('dialog', { name: 'model-a' });
      await drawer.locator('[data-egress-attempt]').first().waitFor();
      assert.equal(await drawer.locator('[data-egress-attempt]').count(), 2);
      assert.ok((await drawer.innerText()).includes(copy.final));
      assert.ok((await drawer.innerText()).includes(copy.noEof));
      assert.match(await drawer.innerText(), /HTTP\/2/);
      assert.match(await drawer.innerText(), /Research account/);
      assert.doesNotMatch(await drawer.innerText(), /credential-canary|attempt-a|attempt-b/);
      await drawer.locator('.close').focus();
      await page.mouse.move(0, 0);
      await page.locator('[role="tooltip"]:visible').waitFor({ state: 'hidden' });
      await drawer.getByRole('region', { name: copy.attemptsTitle, exact: true }).evaluate(element => element.scrollIntoView({ block: 'start' }));
      await page.screenshot({ path: `${artifactRoot}/egress-attempts-${locale}-390.png`, fullPage: false });
      const finalAttempt = drawer.locator('[data-egress-attempt="attempt-b"]');
      await finalAttempt.locator('h4').evaluate(element => element.scrollIntoView({ block: 'start' }));
      await page.screenshot({ path: `${artifactRoot}/egress-attempts-final-${locale}-390.png`, fullPage: false });
      await finalAttempt.getByRole('button', { name: copy.technical, exact: true }).evaluate(element => element.scrollIntoView({ block: 'end' }));
      await page.screenshot({ path: `${artifactRoot}/egress-attempts-final-bottom-${locale}-390.png`, fullPage: false });
      const finalFactsVisible = await finalAttempt.evaluate(element => {
        const facts = element.querySelector('.egress-facts')!.getBoundingClientRect();
        const completion = element.querySelector('p')!.getBoundingClientRect();
        const drawerBounds = element.closest('[role="dialog"]')!.getBoundingClientRect();
        return { top: facts.top, bottom: completion.bottom, viewportTop: Math.max(0, drawerBounds.top), viewportBottom: Math.min(innerHeight, drawerBounds.bottom) };
      });
      assert.ok(finalFactsVisible.top >= finalFactsVisible.viewportTop && finalFactsVisible.bottom <= finalFactsVisible.viewportBottom,
        `the final-attempt bottom frame shows both directional byte values and completion: ${JSON.stringify(finalFactsVisible)}`);
      await finalAttempt.getByRole('button', { name: copy.technical, exact: true }).click();
      await finalAttempt.getByText('attempt-b', { exact: true }).waitFor();
      const identifiers = finalAttempt.locator('.egress-facts').last();
      await identifiers.evaluate(element => element.scrollIntoView({ block: 'center' }));
      await page.mouse.move(0, 0);
      await page.locator('[role="tooltip"]:visible').waitFor({ state: 'hidden' });
      await page.screenshot({ path: `${artifactRoot}/egress-attempts-final-identifiers-${locale}-390.png`, fullPage: false });
      const identifiersVisible = await identifiers.evaluate(element => {
        const bounds = element.getBoundingClientRect();
        const drawerBounds = element.closest('[role="dialog"]')!.getBoundingClientRect();
        return { top: bounds.top, bottom: bounds.bottom, viewportTop: Math.max(0, drawerBounds.top), viewportBottom: Math.min(innerHeight, drawerBounds.bottom), scrollWidth: element.scrollWidth, clientWidth: element.clientWidth };
      });
      assert.ok(identifiersVisible.top >= identifiersVisible.viewportTop && identifiersVisible.bottom <= identifiersVisible.viewportBottom && identifiersVisible.scrollWidth <= identifiersVisible.clientWidth,
        `all expanded troubleshooting identifiers fit in their own frame: ${JSON.stringify(identifiersVisible)}`);
      assert.deepEqual(errors, []);
      await page.close();
    }
  } finally { await browser.close(); await server.close(); }
});

test('request list exit lines render only real recorded exits with exact text and the preserved tooltip', { timeout: 90_000 }, async () => {
  const server = await createIsolatedFixtureServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const origin = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  const artifactRoot = fileURLToPath(new URL('../e2e-artifacts/request-diagnostics/', import.meta.url));
  await mkdir(artifactRoot, { recursive: true });
  try {
    for (const locale of ['zh-CN', 'en']) {
      const zh = locale === 'zh-CN';
      const page = await browser.newPage({ viewport: { width: 390, height: 844 } });
      const errors: string[] = [];
      page.on('pageerror', error => errors.push(error.message));
      await page.route('**/*', route => new URL(route.request().url()).origin === origin ? route.continue() : route.abort());
      await page.addInitScript(value => localStorage.setItem('mtc-locale', value), locale);
      await page.goto(`${origin}/e2e/fixtures/request-lifecycle.html?egress-lines&quiet-background-help`);
      const row = (model: string) => page.locator('tbody tr').filter({ has: page.getByText(model, { exact: true }) });
      const tooltipForExit = async (model: string) => {
        const anchor = row(model).locator('.request-outcome');
        const trigger = row(model).locator('.request-egress-line');
        await page.mouse.move(-1, -1);
        await anchor.focus();
        assert.equal(await anchor.evaluate(element => document.activeElement === element), true, `${model}: keyboard traversal starts at the status`);
        await page.keyboard.press('Escape');
        await page.locator('[role="tooltip"]:visible').waitFor({ state: 'hidden' });
        await page.keyboard.press('Tab');
        assert.equal(await trigger.evaluate(element => document.activeElement === element), true, `${model}: Tab focuses the exact exit trigger`);
        await page.waitForFunction(element => (element?.getAttribute('aria-describedby') ?? '').split(/\s+/).some(id => document.getElementById(id)?.getAttribute('role') === 'tooltip'), await trigger.elementHandle());
        const tooltipId = await trigger.evaluate(element => (element.getAttribute('aria-describedby') ?? '').split(/\s+/).find(id => document.getElementById(id)?.getAttribute('role') === 'tooltip'));
        assert.ok(tooltipId, `${model}: exit trigger describes its own tooltip`);
        const tooltip = page.locator(`[role="tooltip"][id=${JSON.stringify(tooltipId)}]`);
        await tooltip.waitFor({ state: 'visible' });
        return tooltip;
      };
      const proxyLine = row('model-c').locator('.request-egress-line');
      await proxyLine.waitFor();
      assert.equal(await proxyLine.innerText(), 'edge-01 · socks5h://192.0.2.10:1080');
      assert.equal(await proxyLine.getAttribute('data-egress'), 'proxy');
      const directLine = row('model-d').locator('.request-egress-line');
      assert.equal(await directLine.innerText(), zh ? '直连' : 'Direct');
      assert.equal(await directLine.getAttribute('data-egress'), 'direct');
      for (const model of ['model-a', 'model-b', 'model-e', 'model-f', 'model-g']) {
        assert.equal(await row(model).locator('.request-egress-line').count(), 0, `${model} has no recorded exit line`);
      }
      await page.screenshot({ path: `${artifactRoot}/egress-list-lines-${locale}-390.png`, fullPage: true });
      const proxyTooltip = await tooltipForExit('model-c');
      const proxyTip = await proxyTooltip.innerText();
      assert.match(proxyTip, zh ? /代理组: 示例出口组/ : /Proxy group: 示例出口组/);
      assert.match(proxyTip, zh ? /组版本 3/ : /Group version 3/);
      assert.match(proxyTip, /edge-01/);
      assert.match(proxyTip, /socks5h:\/\/192\.0\.2\.10:1080/);
      assert.match(proxyTip, zh ? /连通性当时没有测量/ : /Connectivity was not measured for this request/);
      assert.match(proxyTip, zh ? /这次请求走代理出口/ : /This request used a proxy exit/);
      await page.keyboard.press('Escape');
      await proxyTooltip.waitFor({ state: 'hidden' });
      const directTooltip = await tooltipForExit('model-d');
      assert.match(await directTooltip.innerText(), zh ? /这次请求走直连/ : /This request used a direct connection/);
      await page.keyboard.press('Escape');
      await directTooltip.waitFor({ state: 'hidden' });
      assert.deepEqual(errors, []);
      await page.close();
    }
  } finally { await browser.close(); await server.close(); }
});
