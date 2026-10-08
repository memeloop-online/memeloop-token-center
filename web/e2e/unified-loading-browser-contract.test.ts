import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium, type Page } from 'playwright';
import { createIsolatedFixtureServer as createServer } from './support/isolated-vite-server.js';

function deferred() {
  let release!: () => void;
  const pending = new Promise<void>((resolve) => { release = resolve; });
  return { pending, release };
}

async function layout(page: Page) {
  return page.evaluate(() => {
    const selectors = ['h1', '[data-page-loading-announcement]', '[data-loading-surface="primary"] .mtc-resource-boundary', '[data-loading-surface="secondary"]'];
    return Object.fromEntries(selectors.map((selector) => {
      const bounds = document.querySelector(selector)!.getBoundingClientRect();
      return [selector, { x: bounds.x, y: bounds.y, width: bounds.width, height: bounds.height }];
    }));
  });
}

function assertStable(before: Awaited<ReturnType<typeof layout>>, after: Awaited<ReturnType<typeof layout>>, phase: string) {
  for (const selector of Object.keys(before)) for (const coordinate of ['x', 'y', 'width', 'height'] as const) {
    assert.ok(Math.abs(before[selector][coordinate] - after[selector][coordinate]) <= 1,
      `${phase}: ${selector} ${coordinate} changed ${before[selector][coordinate]} -> ${after[selector][coordinate]}`);
  }
}

test('shared loading retains coordinates, one announcement and scoped data across slow resources and refresh', { timeout: 120_000 }, async (context) => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required for unified loading acceptance');
    context.skip('Chromium is required'); return;
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  const artifacts = fileURLToPath(new URL('../e2e-artifacts/ui-system/loading/', import.meta.url));
  await mkdir(artifacts, { recursive: true });
  try {
    for (const locale of ['en', 'zh-CN']) for (const width of [1440, 390]) {
      const page = await browser.newPage({ viewport: { width, height: 900 } });
      const prefix = `shared-region-${locale}-${width}`;
      const loadingLabel = locale === 'en' ? 'Loading…' : '载入中…';
      const retryLabel = locale === 'en' ? 'Retry' : '重试';
      const noMatches = locale === 'en' ? 'No matching resources' : '没有符合条件的资源';
      const refreshError = locale === 'en' ? 'The refresh failed. Try again.' : '刷新失败，请重试。';
      const deniedError = locale === 'en' ? 'Read access is unavailable for this tenant.' : '没有此租户的读取权限。';
      const errors: string[] = [];
      const requests: string[] = [];
      const screenshots: string[] = [];
      const gates = new Map<string, ReturnType<typeof deferred>>();
      const hold = (identity: string) => { const gate = deferred(); gates.set(identity, gate); return gate; };
      const primaryInitial = hold('alpha:one:primary:0');
      const secondaryInitial = hold('alpha:one:secondary:0');
      const tertiaryInitial = hold('alpha:one:tertiary:0');
      let denied = false;
      page.on('pageerror', (error) => errors.push(error.message));
      await page.addInitScript((value) => localStorage.setItem('mtc-locale', value), locale);
      await page.route('**/fixture/loading?*', async (route) => {
        const url = new URL(route.request().url());
        const tenant = url.searchParams.get('tenant');
        const credential = url.searchParams.get('credential');
        const resource = url.searchParams.get('resource');
        const revision = url.searchParams.get('revision');
        const identity = `${tenant}:${credential}:${resource}:${revision}`;
        requests.push(identity);
        const gate = gates.get(identity);
        if (gate) await gate.pending;
        const failed = resource === 'primary' && url.searchParams.get('outcome') === 'failed';
        await route.fulfill(denied && resource === 'primary'
          ? { status: 403, json: { error: { message: deniedError } } }
          : failed
            ? { status: 503, json: { error: { message: refreshError } } }
            : { json: { tenant, credential, records: url.searchParams.get('outcome') === 'empty' ? [] : [`${tenant}-${credential}-${resource}`] } }
        ).catch(() => undefined);
      });
      const screenshot = async (state: string) => {
        const name = `${prefix}-${state}.png`;
        await page.screenshot({ path: `${artifacts}/${name}`, fullPage: true });
        screenshots.push(name);
      };
      const action = (name: string) => page.locator(`[data-fixture-action="${name}"]`);
      const primary = page.locator('[data-ready-resource="primary"]');
      const draft = page.locator('[data-workspace-draft]');
      const announcement = page.locator('[data-page-loading-announcement]');
      try {
        await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/resource-loading.html?mode=unified&ignore-abort=1`);
        await announcement.getByText(loadingLabel, { exact: true }).waitFor();
        assert.equal(await announcement.count(), 1, 'nested loading regions reuse the page announcement');
        assert.equal(await page.getByRole('status').count(), 1, 'skeletons do not add live announcements');
        assert.equal(await page.getByText(loadingLabel, { exact: true }).count(), 1, 'one visible loading label, not one per slow resource');
        assert.equal(await page.locator('.mtc-loading-state').count(), 3);
        const initial = await layout(page);
        assert.equal(initial['[data-page-loading-announcement]'].height, 32);
        assert.equal(initial['[data-page-loading-announcement]'].x, Math.max(0, (width - 1200) / 2) + 16);
        await screenshot('initial-slow');

        primaryInitial.release();
        await primary.waitFor();
        await page.waitForFunction(() => document.querySelector('[data-page-loading-announcement]')?.textContent === '');
        assert.equal(await page.locator('.mtc-loading-state').count(), 2, 'optional reads remain local after the primary becomes usable');
        assertStable(initial, await layout(page), 'primary ready with two slow sections');
        await action('available').click();
        assert.equal(await page.locator('[data-action-count]').textContent(), '1', 'slow optional resources do not block available actions');
        await screenshot('primary-ready-local-slow');

        secondaryInitial.release();
        tertiaryInitial.release();
        await page.locator('[data-ready-resource="secondary"]').waitFor();
        await page.locator('[data-ready-resource="tertiary"]').waitFor();
        assertStable(initial, await layout(page), 'all resources ready');
        await screenshot('normal');

        await draft.fill('unsaved workspace draft');
        await draft.evaluate((element) => { element.setAttribute('data-preserved-instance', 'original'); });
        const refreshPending = hold('alpha:one:primary:1');
        await action('refresh').click();
        await announcement.getByText(loadingLabel, { exact: true }).waitFor();
        await draft.focus();
        assert.equal(await primary.getAttribute('data-ready-scope'), 'alpha-one');
        assert.equal(await draft.inputValue(), 'unsaved workspace draft');
        assert.equal(await draft.getAttribute('data-preserved-instance'), 'original');
        assert.equal(await page.locator('.mtc-loading-state').count(), 0, 'same-scope refresh keeps data, not skeletons');
        assertStable(initial, await layout(page), 'same-scope refresh');
        await screenshot('refresh-preserves-workspace');
        refreshPending.release();
        await page.waitForFunction(() => document.querySelector('[data-page-loading-announcement]')?.textContent === '');
        assert.equal(await draft.evaluate((element) => element === document.activeElement), true, 'refresh completion preserves keyboard focus');
        assert.equal(await draft.getAttribute('data-preserved-instance'), 'original');

        await action('fail').click();
        await page.getByRole('alert').filter({ hasText: refreshError }).waitFor();
        assert.equal(await primary.getAttribute('data-ready-scope'), 'alpha-one');
        assert.equal(await draft.inputValue(), 'unsaved workspace draft');
        assert.equal(await draft.getAttribute('data-preserved-instance'), 'original');
        assert.equal(await announcement.textContent(), '', 'failed refresh stops the loading announcement');
        assert.equal(requests.filter((identity) => identity === 'alpha:one:primary:2').length, 1, 'no retry is added by shared loading');
        await screenshot('refresh-failed-retained');

        await action('empty').click();
        await page.getByText(noMatches, { exact: true }).waitFor();
        await page.waitForFunction(() => document.querySelector('[data-page-loading-announcement]')?.textContent === '');
        assert.equal(await page.getByRole('alert').count(), 0);
        assert.equal(await page.locator('.mtc-loading-state').count(), 0, 'an acknowledged empty value is not a load');
        await screenshot('empty');

        const lateAlpha = hold('alpha:one:primary:4');
        await action('refresh').click();
        await announcement.getByText(loadingLabel, { exact: true }).waitFor();
        const betaPending = hold('beta:one:primary:4');
        await action('tenant').click();
        assert.equal(await page.locator('[data-ready-scope="alpha-one"]').count(), 0, 'tenant change discards every old visible resource immediately');
        assert.equal(await draft.count(), 0, 'tenant change also discards old uncontrolled draft state');
        await announcement.getByText(loadingLabel, { exact: true }).waitFor();
        await screenshot('tenant-switch-slow');
        const oldResponse = page.waitForResponse((response) => {
          const url = new URL(response.url());
          return url.pathname === '/fixture/loading' && url.searchParams.get('tenant') === 'alpha'
            && url.searchParams.get('resource') === 'primary' && url.searchParams.get('revision') === '4';
        });
        lateAlpha.release();
        await (await oldResponse).finished();
        await page.evaluate(() => new Promise<void>((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
        assert.equal(await page.locator('[data-ready-scope="alpha-one"]').count(), 0, 'a noncooperative old read cannot repopulate the new tenant');
        betaPending.release();
        await page.locator('[data-ready-resource="primary"][data-ready-scope="beta-one"]').waitFor();
        assert.equal(await draft.inputValue(), '');

        const credentialPending = hold('beta:two:primary:4');
        await action('credential').click();
        assert.equal(await page.locator('[data-ready-scope="beta-one"]').count(), 0, 'credential changes isolate old data even in the same tenant');
        await announcement.getByText(loadingLabel, { exact: true }).waitFor();
        await screenshot('credential-switch-slow');
        credentialPending.release();
        await page.locator('[data-ready-resource="primary"][data-ready-scope="beta-two"]').waitFor();
        await action('access').click();
        assert.equal(await page.locator('[data-ready-resource]').count(), 0, 'disabled authority hides ready values in the same scope');
        await page.waitForFunction(() => document.querySelector('[data-page-loading-announcement]')?.textContent === '');
        assert.equal(await page.locator('.mtc-loading-state').count(), 0, 'disabled reads are not announced as indefinitely loading');
        denied = true;
        await action('access').click();
        await page.getByRole('alert').filter({ hasText: deniedError }).waitFor();
        await page.locator('[data-ready-resource="secondary"][data-ready-scope="beta-two"]').waitFor();
        await page.locator('[data-ready-resource="tertiary"][data-ready-scope="beta-two"]').waitFor();
        assert.equal(await primary.count(), 0, 'an initial permission failure never reuses a prior ready value');
        assert.equal(await page.locator('.mtc-loading-state').count(), 0, 'completed failure is not shown as indefinite loading');
        await screenshot('permission-failed');
        denied = false;
        await page.getByRole('button', { name: retryLabel, exact: true }).click();
        await page.locator('[data-ready-resource="primary"][data-ready-scope="beta-two"]').waitFor();
        await page.waitForFunction(() => document.querySelector('[data-page-loading-announcement]')?.textContent === '');
        await screenshot('permission-recovered');

        await page.emulateMedia({ reducedMotion: 'reduce' });
        const reducedPending = hold('beta:two:primary:5');
        await action('refresh').click();
        await announcement.getByText(loadingLabel, { exact: true }).waitFor();
        const animations = await page.locator('.mtc-loading-track').evaluateAll((tracks) => tracks.flatMap((track) => [...track.querySelectorAll('*')].map((element) => getComputedStyle(element, '::after').animationName)));
        assert.ok(animations.every((name) => name === 'none'), 'loading honors reduced motion');
        reducedPending.release();
        await page.waitForFunction(() => document.querySelector('[data-page-loading-announcement]')?.textContent === '');
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true, 'loading and errors remain inside the narrow viewport');
        assert.deepEqual(errors, []);
        await writeFile(`${artifacts}/${prefix}-proof.json`, JSON.stringify({ status: 'PASS', locale, width, coordinates: initial, requests, screenshots, errors }, null, 2));
      } catch (reason) {
        await screenshot('failure-evidence').catch(() => undefined);
        await writeFile(`${artifacts}/${prefix}-proof.json`, JSON.stringify({ status: 'FAIL', locale, width, requests, screenshots, errors, failure: reason instanceof Error ? reason.message : String(reason) }, null, 2));
        throw reason;
      } finally {
        gates.forEach((gate) => gate.release());
        await page.close();
      }
    }
  } finally {
    await browser.close();
    await server.close();
  }
});

test('concurrent page reads share one announcement without gating a completed resource or optional section', { timeout: 60_000 }, async (context) => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required for concurrent loading acceptance');
    context.skip('Chromium is required'); return;
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  const page = await browser.newPage({ viewport: { width: 390, height: 900 } });
  const gates = { primary: deferred(), secondary: deferred(), tertiary: deferred() };
  const artifacts = fileURLToPath(new URL('../e2e-artifacts/ui-system/loading/', import.meta.url));
  await mkdir(artifacts, { recursive: true });
  const errors: string[] = [];
  page.on('pageerror', (error) => errors.push(error.message));
  await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
  await page.route('**/fixture/loading?*', async (route) => {
    const url = new URL(route.request().url());
    const resource = url.searchParams.get('resource');
    assert.ok(resource === 'primary' || resource === 'secondary' || resource === 'tertiary');
    await gates[resource].pending;
    await route.fulfill({ json: { tenant: 'alpha', credential: 'one', records: [`alpha-one-${resource}`] } }).catch(() => undefined);
  });
  const screenshots: string[] = [];
  const screenshot = async (state: string) => {
    const name = `multi-page-en-390-${state}.png`;
    await page.screenshot({ path: `${artifacts}/${name}`, fullPage: true });
    screenshots.push(name);
  };
  try {
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/resource-loading.html?mode=unified&secondary-page=1`);
    const announcement = page.locator('[data-page-loading-announcement]');
    await announcement.getByText('Loading…', { exact: true }).waitFor();
    assert.equal(await page.getByRole('status').count(), 1);
    const initial = await layout(page);
    await screenshot('initial-slow');
    gates.primary.release();
    await page.locator('[data-ready-resource="primary"]').waitFor();
    assert.equal(await announcement.textContent(), 'Loading…', 'the second page-level read retains the single pending announcement');
    assert.equal(await page.getByText('Loading…', { exact: true }).count(), 1);
    await page.locator('[data-fixture-action="available"]').click();
    assert.equal(await page.locator('[data-action-count]').textContent(), '1', 'page pending aggregation is not a request/action gate');
    assertStable(initial, await layout(page), 'one page-level read ready');
    await screenshot('one-page-ready');
    gates.secondary.release();
    await page.locator('[data-ready-resource="secondary"]').waitFor();
    await page.waitForFunction(() => document.querySelector('[data-page-loading-announcement]')?.textContent === '');
    assert.equal(await page.locator('.mtc-loading-state').count(), 1, 'the optional history remains local after both page prerequisites finish');
    assertStable(initial, await layout(page), 'only optional section pending');
    await screenshot('local-only-slow');
    gates.tertiary.release();
    await page.locator('[data-ready-resource="tertiary"]').waitFor();
    assertStable(initial, await layout(page), 'all concurrent reads ready');
    await screenshot('normal');
    assert.deepEqual(errors, []);
    await writeFile(`${artifacts}/multi-page-en-390-proof.json`, JSON.stringify({ status: 'PASS', coordinates: initial, screenshots, errors }, null, 2));
  } catch (reason) {
    await screenshot('failure-evidence').catch(() => undefined);
    await writeFile(`${artifacts}/multi-page-en-390-proof.json`, JSON.stringify({ status: 'FAIL', screenshots, errors, failure: reason instanceof Error ? reason.message : String(reason) }, null, 2));
    throw reason;
  } finally {
    Object.values(gates).forEach((gate) => gate.release());
    await browser.close();
    await server.close();
  }
});
