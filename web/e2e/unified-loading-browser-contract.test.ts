import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync } from 'node:fs';
import { mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium, type Page } from 'playwright';
import { createIsolatedFixtureServer as createServer } from './support/isolated-vite-server.js';

const artifacts = fileURLToPath(new URL('../e2e-artifacts/ui-system/loading/', import.meta.url));
const repository = fileURLToPath(new URL('../..', import.meta.url));
const clock = '2026-10-09T00:00:00.000Z';
const viewports = [{ width: 1440, height: 1000 }, { width: 390, height: 844 }];
const locales = ['en', 'zh-CN'];
const scenarios = ['initial-loading', 'local-slow', 'ready', 'background-refresh', 'refresh-error', 'empty', 'tenant-switch-slow', 'credential-switch-slow', 'permission-failure', 'permission-recovered', 'error'];
const concurrentScenarios = ['initial-loading', 'one-page-ready', 'local-only-slow', 'ready'];
const sourceFiles = ['web/src/design-system/loading.tsx', 'web/src/design-system/loading.css', 'web/src/operator/ResourceBoundary.tsx', 'web/src/operator/hooks/useOperatorResource.ts', 'web/e2e/fixtures/resource-loading.tsx', 'web/e2e/unified-loading-browser-contract.test.ts'];
type Evidence = {
  surface: string; scenario: string; locale: string; viewport: { width: number; height: number }; theme: string;
  file: string; generation_status: 'missing' | 'generated' | 'failed'; file_sha256?: string; assertions: string[];
  geometry?: Awaited<ReturnType<typeof geometry>>; failure?: string;
};
const filename = (surface: string, scenario: string, locale: string, viewport: { width: number; height: number }, theme: string) => `${surface}--${scenario}--${locale}--${viewport.width}x${viewport.height}--${theme}.png`;
const evidence: Evidence[] = [];
for (const locale of locales) for (const viewport of viewports) {
  for (const scenario of scenarios) evidence.push({ surface: 'shared-region', scenario, locale, viewport, theme: 'light', file: filename('shared-region', scenario, locale, viewport, 'light'), generation_status: 'missing', assertions: [] });
  evidence.push({ surface: 'shared-region', scenario: 'ready', locale, viewport, theme: 'dark', file: filename('shared-region', 'ready', locale, viewport, 'dark'), generation_status: 'missing', assertions: [] });
  evidence.push({ surface: 'inline-row', scenario: 'initial-loading', locale, viewport, theme: 'light', file: filename('inline-row', 'initial-loading', locale, viewport, 'light'), generation_status: 'missing', assertions: [] });
}
for (const scenario of concurrentScenarios) evidence.push({ surface: 'concurrent-page', scenario, locale: 'en', viewport: viewports[1], theme: 'light', file: filename('concurrent-page', scenario, 'en', viewports[1], 'light'), generation_status: 'missing', assertions: [] });
const git = (...args: string[]) => execFileSync('git', args, { cwd: repository, encoding: 'utf8' }).trim();
let provenance: Record<string, unknown> | undefined;

async function saveManifest() {
  await mkdir(artifacts, { recursive: true });
  if (!provenance) {
    const checkout = git('rev-parse', 'HEAD');
    const integrated = process.env.GITHUB_SHA || checkout;
    const commits = Object.fromEntries(sourceFiles.map((file) => [file, git('log', '-1', '--format=%H', '--', file)]));
    provenance = {
      evidence_kind: 'synthetic', base_sha: '8fa93513f31d0e895351a8126f317bf2f88e8b81', integrated_head_sha: integrated,
      checkout_sha: checkout, checkout_matches_integrated: checkout === integrated,
      owner_heads: { loading_source: 'fd9f55ec89d2274f3134f5b2167f2d8480be5cef', loading_tests_applied: commits['web/e2e/unified-loading-browser-contract.test.ts'], account: '89919272f4c27a8873be2c5ecb9a89721bd7d48b', navigation: '7a8d8e61b1378d9df653cc78e3d472780def9a18', shared_styles: '64a19112406522b20c06d9169e6c1a4a4fe64bca' },
      owner_head_basis: 'submitted frozen source heads; applied_source_commits and exact source bytes identify any integration changes',
      applied_source_commits: commits,
      source_sha256: Object.fromEntries(sourceFiles.map((file) => [file, createHash('sha256').update(execFileSync('git', ['show', `HEAD:${file}`], { cwd: repository })).digest('hex')])),
      ci_run_url: process.env.GITHUB_RUN_ID ? `${process.env.GITHUB_SERVER_URL}/${process.env.GITHUB_REPOSITORY}/actions/runs/${process.env.GITHUB_RUN_ID}` : null,
      job: process.env.GITHUB_JOB || null, artifact: `monitoring-interactions-browser-${integrated}`,
      writer: 'web/e2e/unified-loading-browser-contract.test.ts', fixture: 'web/e2e/fixtures/resource-loading.tsx', source: sourceFiles,
      seed: 'loading-alpha-beta-one-two-v1', clock, device_scale_factor: 1, reduced_motion: 'reduce',
      matrix_scope: 'shared loading only; account-workspace and navigation-identity are separate writers, not claimed here',
      geometry_scope: 'shared loading anchors and actual buttons; no table column tracks exist in this fixture',
    };
  }
  await writeFile(`${artifacts}/manifest.json`, JSON.stringify({ ...provenance, expected_screenshots: 56, screenshots: evidence }, null, 2));
  assert.equal(provenance.checkout_matches_integrated, true, 'artifact must describe the exact checkout tested by GHA');
}

async function geometry(page: Page) {
  return page.evaluate(() => {
    const buttons = [...document.querySelectorAll<HTMLButtonElement>('button[data-fixture-action]')].map((element) => {
      const bounds = element.getBoundingClientRect();
      return { action: element.dataset.fixtureAction, x: bounds.x, y: bounds.y, width: bounds.width, height: bounds.height };
    });
    const heights = buttons.map((button) => button.height);
    return {
      buttons, button_height_delta: heights.length ? Math.max(...heights) - Math.min(...heights) : 0,
      narrow_targets_valid: innerWidth > 768 || buttons.every((button) => button.width >= 44 && button.height >= 44),
      viewport_contained: document.documentElement.scrollWidth <= innerWidth,
    };
  });
}

async function capture(page: Page, surface: string, scenario: string, locale: string, viewport: { width: number; height: number }, screenshots: string[], assertions: string[], geometryFailures: string[], theme = 'light') {
  const file = filename(surface, scenario, locale, viewport, theme);
  let entry = evidence.find((value) => value.file === file);
  if (!entry) {
    entry = { surface, scenario, locale, viewport, theme, file, generation_status: 'missing', assertions: [] };
    evidence.push(entry);
  }
  try {
    await page.evaluate(async () => { await document.fonts.ready; window.scrollTo(0, 0); });
    await page.mouse.move(0, 0);
    await page.evaluate(() => new Promise<void>((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
    const measured = await geometry(page);
    const issues = [
      ...(measured.button_height_delta > 1 ? ['same-mode button height delta exceeds 1px'] : []),
      ...(!measured.narrow_targets_valid ? ['narrow interactive targets are smaller than 44x44px'] : []),
      ...(!measured.viewport_contained ? ['document overflows viewport'] : []),
    ];
    geometryFailures.push(...issues.map((issue) => `${file}: ${issue}`));
    const bytes = await page.screenshot({ path: `${artifacts}/${file}`, fullPage: true, animations: 'disabled' });
    screenshots.push(file);
    Object.assign(entry, { generation_status: issues.length ? 'failed' : 'generated', file_sha256: createHash('sha256').update(bytes).digest('hex'), assertions, geometry: measured, ...(issues.length ? { failure: issues.join('; ') } : {}) });
  } catch (reason) {
    Object.assign(entry, { generation_status: 'failed', failure: reason instanceof Error ? reason.message : String(reason) });
    throw reason;
  } finally {
    await saveManifest();
  }
}

async function initializePage(page: Page, locale: string) {
  await page.clock.setFixedTime(new Date(clock));
  await page.addInitScript((value) => {
    localStorage.setItem('mtc-locale', value);
    localStorage.setItem('mtc-theme', 'light');
    if (document.documentElement) {
      document.documentElement.dataset.theme = 'light';
      document.documentElement.lang = value;
    } else document.addEventListener('DOMContentLoaded', () => {
      document.documentElement.dataset.theme = 'light';
      document.documentElement.lang = value;
    }, { once: true });
  }, locale);
}

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
  await saveManifest();
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required for unified loading acceptance');
    context.skip('Chromium is required'); return;
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  const caseFailures: string[] = [];
  try {
    for (const locale of locales) for (const viewport of viewports) {
      const { width, height } = viewport;
      const page = await browser.newPage({ viewport, deviceScaleFactor: 1, reducedMotion: 'reduce', colorScheme: 'light' });
      const prefix = `shared-region--${locale}--${width}x${height}`;
      const loadingLabel = locale === 'en' ? 'Loading…' : '载入中…';
      const retryLabel = locale === 'en' ? 'Retry' : '重试';
      const noMatches = locale === 'en' ? 'No matching resources' : '没有符合条件的资源';
      const refreshError = locale === 'en' ? 'The refresh failed. Try again.' : '刷新失败，请重试。';
      const deniedError = locale === 'en' ? 'Read access is unavailable for this tenant.' : '没有此租户的读取权限。';
      const errors: string[] = [];
      const requests: string[] = [];
      const screenshots: string[] = [];
      const geometryFailures: string[] = [];
      const gates = new Map<string, ReturnType<typeof deferred>>();
      const hold = (identity: string) => { const gate = deferred(); gates.set(identity, gate); return gate; };
      const primaryInitial = hold('alpha:one:primary:0');
      const secondaryInitial = hold('alpha:one:secondary:0');
      const tertiaryInitial = hold('alpha:one:tertiary:0');
      let denied = false;
      let initialError = false;
      page.on('pageerror', (error) => errors.push(error.message));
      await initializePage(page, locale);
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
        const failed = resource === 'primary' && (initialError || url.searchParams.get('outcome') === 'failed');
        await route.fulfill(denied && resource === 'primary'
          ? { status: 403, json: { error: { message: deniedError } } }
          : failed
            ? { status: 503, json: { error: { message: refreshError } } }
            : { json: { tenant, credential, records: url.searchParams.get('outcome') === 'empty' ? [] : [`${tenant}-${credential}-${resource}`] } }
        ).catch(() => undefined);
      });
      const screenshot = (state: string, assertions: string[] = [], theme = 'light') => capture(page, 'shared-region', state, locale, viewport, screenshots, assertions, geometryFailures, theme);
      const action = (name: string) => page.locator(`[data-fixture-action="${name}"]`);
      const primary = page.locator('[data-ready-resource="primary"]');
      const draft = page.locator('[data-workspace-draft]');
      const announcement = page.locator('[data-page-loading-announcement]');
      try {
        await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/resource-loading.html?mode=unified&ignore-abort=1`);
        await announcement.getByText(loadingLabel, { exact: true }).waitFor();
        await page.evaluate(async () => { await document.fonts.ready; });
        assert.equal(await announcement.count(), 1, 'nested loading regions reuse the page announcement');
        assert.equal(await page.getByRole('status').count(), 1, 'skeletons do not add live announcements');
        assert.equal(await page.getByText(loadingLabel, { exact: true }).count(), 1, 'one visible loading label, not one per slow resource');
        assert.equal(await page.locator('.mtc-loading-state').count(), 3);
        const initial = await layout(page);
        assert.equal(initial['[data-page-loading-announcement]'].height, 32);
        assert.equal(initial['[data-page-loading-announcement]'].x, Math.max(0, (width - 1200) / 2) + 16);
        await screenshot('initial-loading', ['one-page-status', 'three-local-skeletons', 'reserved-rail-32px', 'fixed-page-coordinate']);

        primaryInitial.release();
        await primary.waitFor();
        await page.waitForFunction(() => document.querySelector('[data-page-loading-announcement]')?.textContent === '');
        assert.equal(await page.locator('.mtc-loading-state').count(), 2, 'optional reads remain local after the primary becomes usable');
        assertStable(initial, await layout(page), 'primary ready with two slow sections');
        await action('available').click();
        assert.equal(await page.locator('[data-action-count]').textContent(), '1', 'slow optional resources do not block available actions');
        await screenshot('local-slow', ['optional-reads-do-not-block-actions', 'no-local-live-loading-text', 'anchors-stable-within-1px']);

        secondaryInitial.release();
        tertiaryInitial.release();
        await page.locator('[data-ready-resource="secondary"]').waitFor();
        await page.locator('[data-ready-resource="tertiary"]').waitFor();
        assertStable(initial, await layout(page), 'all resources ready');
        await screenshot('ready', ['all-actual-boundaries-ready', 'anchors-stable-within-1px']);
        await page.evaluate(() => { document.documentElement.dataset.theme = 'dark'; });
        await screenshot('ready', ['same-components-alternate-theme'], 'dark');
        await page.evaluate(() => { document.documentElement.dataset.theme = 'light'; });

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
        assert.equal(await draft.evaluate((element) => element === document.activeElement), true);
        await screenshot('background-refresh', ['same-dom-instance', 'draft-retained', 'keyboard-focus-retained', 'no-ready-content-skeleton', 'anchors-stable-within-1px']);
        refreshPending.release();
        await page.waitForFunction(() => document.querySelector('[data-page-loading-announcement]')?.textContent === '');
        assert.equal(await draft.evaluate((element) => element === document.activeElement), true, 'refresh completion preserves keyboard focus');
        assert.equal(await draft.getAttribute('data-preserved-instance'), 'original');

        await action('fail').click();
        await page.getByRole('alert').filter({ hasText: refreshError }).waitFor();
        assert.equal(await page.getByRole('alert').count(), 1, 'refresh failure has one feedback owner');
        assert.equal(await primary.getAttribute('data-ready-scope'), 'alpha-one');
        assert.equal(await draft.inputValue(), 'unsaved workspace draft');
        assert.equal(await draft.getAttribute('data-preserved-instance'), 'original');
        assert.equal(await announcement.textContent(), '', 'failed refresh stops the loading announcement');
        assert.equal(requests.filter((identity) => identity === 'alpha:one:primary:2').length, 1, 'no retry is added by shared loading');
        await screenshot('refresh-error', ['same-dom-and-draft-retained', 'one-error-feedback', 'no-automatic-retry']);

        await action('empty').click();
        await page.getByText(noMatches, { exact: true }).waitFor();
        await page.waitForFunction(() => document.querySelector('[data-page-loading-announcement]')?.textContent === '');
        assert.equal(await page.getByRole('alert').count(), 0);
        assert.equal(await page.locator('.mtc-loading-state').count(), 0, 'an acknowledged empty value is not a load');
        await screenshot('empty', ['acknowledged-empty-not-loading', 'no-error']);

        const lateAlpha = hold('alpha:one:primary:4');
        await action('refresh').click();
        await announcement.getByText(loadingLabel, { exact: true }).waitFor();
        const betaPending = hold('beta:one:primary:4');
        await action('tenant').click();
        assert.equal(await page.locator('[data-ready-scope="alpha-one"]').count(), 0, 'tenant change discards every old visible resource immediately');
        assert.equal(await draft.count(), 0, 'tenant change also discards old uncontrolled draft state');
        await announcement.getByText(loadingLabel, { exact: true }).waitFor();
        await screenshot('tenant-switch-slow', ['old-scope-and-draft-synchronously-hidden']);
        const oldResponse = page.waitForResponse((response) => {
          const url = new URL(response.url());
          return url.pathname === '/fixture/loading' && url.searchParams.get('tenant') === 'alpha'
            && url.searchParams.get('resource') === 'primary' && url.searchParams.get('revision') === '4';
        });
        lateAlpha.release();
        await (await oldResponse).finished();
        await page.evaluate(() => new Promise<void>((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
        assert.equal(await page.locator('[data-ready-scope="alpha-one"]').count(), 0, 'a noncooperative old read cannot repopulate the new tenant');
        evidence.find((entry) => entry.file === filename('shared-region', 'tenant-switch-slow', locale, viewport, 'light'))!.assertions.push('noncooperative-late-response-fenced');
        betaPending.release();
        await page.locator('[data-ready-resource="primary"][data-ready-scope="beta-one"]').waitFor();
        assert.equal(await draft.inputValue(), '');

        const credentialPending = hold('beta:two:primary:4');
        await action('credential').click();
        assert.equal(await page.locator('[data-ready-scope="beta-one"]').count(), 0, 'credential changes isolate old data even in the same tenant');
        await announcement.getByText(loadingLabel, { exact: true }).waitFor();
        await screenshot('credential-switch-slow', ['same-tenant-old-credential-synchronously-hidden']);
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
        assert.equal(await page.getByRole('alert').count(), 1);
        await screenshot('permission-failure', ['initial-403-no-prior-ready-value', 'one-error-feedback', 'no-indefinite-skeleton']);
        denied = false;
        await page.getByRole('button', { name: retryLabel, exact: true }).click();
        await page.locator('[data-ready-resource="primary"][data-ready-scope="beta-two"]').waitFor();
        await page.waitForFunction(() => document.querySelector('[data-page-loading-announcement]')?.textContent === '');
        await screenshot('permission-recovered', ['explicit-retry-recovers-current-scope']);

        await action('access').click();
        assert.equal(await page.locator('[data-ready-resource]').count(), 0);
        initialError = true;
        await action('access').click();
        await page.getByRole('alert').filter({ hasText: refreshError }).waitFor();
        await page.locator('[data-ready-resource="secondary"]').waitFor();
        await page.locator('[data-ready-resource="tertiary"]').waitFor();
        assert.equal(await primary.count(), 0, 'an initial non-permission failure is not empty or stale data');
        assert.equal(await page.getByRole('alert').count(), 1);
        assert.equal(await page.locator('.mtc-loading-state').count(), 0);
        await screenshot('error', ['no-old-data-or-empty-fallback', 'one-error-feedback', 'local-ready-content-remains-usable']);
        initialError = false;
        await page.getByRole('button', { name: retryLabel, exact: true }).click();
        await primary.waitFor();
        await page.waitForFunction(() => document.querySelector('[data-page-loading-announcement]')?.textContent === '');

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
        assert.deepEqual(geometryFailures, [], 'all completed loading states meet shared geometry contracts');
        await writeFile(`${artifacts}/${prefix}--proof.json`, JSON.stringify({ status: 'PASS', locale, viewport, coordinates: initial, requests, screenshots, errors, geometryFailures }, null, 2));
      } catch (reason) {
        await screenshot('failure-evidence').catch(() => undefined);
        await writeFile(`${artifacts}/${prefix}--proof.json`, JSON.stringify({ status: 'FAIL', locale, viewport, requests, screenshots, errors, geometryFailures, failure: reason instanceof Error ? reason.message : String(reason) }, null, 2));
        caseFailures.push(`${prefix}: ${reason instanceof Error ? reason.message : String(reason)}`);
      } finally {
        gates.forEach((gate) => gate.release());
        await page.close();
      }
    }
    assert.deepEqual(caseFailures, [], 'each locale and viewport must complete its loading contracts');
  } finally {
    await browser.close();
    await server.close();
  }
});

test('inline loading stays in one existing row without a local live announcement', { timeout: 60_000 }, async (context) => {
  await saveManifest();
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required for inline loading acceptance');
    context.skip('Chromium is required'); return;
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    for (const locale of locales) for (const viewport of viewports) {
      const page = await browser.newPage({ viewport, deviceScaleFactor: 1, reducedMotion: 'reduce', colorScheme: 'light' });
      const screenshots: string[] = [];
      const failures: string[] = [];
      const errors: string[] = [];
      page.on('pageerror', (error) => errors.push(error.message));
      try {
        await initializePage(page, locale);
        await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/resource-loading.html?mode=inline`);
        await page.locator('.mtc-loading-inline').waitFor();
        await page.evaluate(async () => { await document.fonts.ready; });
        assert.equal(await page.getByRole('status').count(), 0);
        assert.equal(await page.getByText(locale === 'en' ? 'Loading…' : '载入中…', { exact: true }).count(), 0);
        assert.equal(await page.locator('.mtc-loading-row').count(), 1);
        const bounds = await page.locator('.mtc-loading-inline').boundingBox();
        const column = await page.locator('[data-inline-column]').boundingBox();
        assert.ok(bounds && column);
        assert.ok(bounds.height <= 20, 'one-line placeholders cannot expand into an 88px card');
        assert.ok(Math.abs(bounds.x - column.x) <= 1 && Math.abs(bounds.width - column.width) <= 1, 'placeholder follows its existing column');
        await capture(page, 'inline-row', 'initial-loading', locale, viewport, screenshots, ['one-fluent-skeleton-row', 'no-local-live-status-or-visible-loading-copy', 'height-at-most-20px', 'column-track-within-1px'], failures);
        assert.deepEqual(errors, [], 'inline loading must also initialize without runtime errors');
        assert.deepEqual(failures, []);
      } catch (reason) {
        await capture(page, 'inline-row', 'failure-evidence', locale, viewport, screenshots, [], failures).catch(() => undefined);
        throw reason;
      } finally {
        await page.close();
      }
    }
  } finally {
    await browser.close();
    await server.close();
  }
});

test('concurrent page reads share one announcement without gating a completed resource or optional section', { timeout: 60_000 }, async (context) => {
  await saveManifest();
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required for concurrent loading acceptance');
    context.skip('Chromium is required'); return;
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  const viewport = viewports[1];
  const page = await browser.newPage({ viewport, deviceScaleFactor: 1, reducedMotion: 'reduce', colorScheme: 'light' });
  const gates = { primary: deferred(), secondary: deferred(), tertiary: deferred() };
  const errors: string[] = [];
  page.on('pageerror', (error) => errors.push(error.message));
  await initializePage(page, 'en');
  await page.route('**/fixture/loading?*', async (route) => {
    const url = new URL(route.request().url());
    const resource = url.searchParams.get('resource');
    assert.ok(resource === 'primary' || resource === 'secondary' || resource === 'tertiary');
    await gates[resource].pending;
    await route.fulfill({ json: { tenant: 'alpha', credential: 'one', records: [`alpha-one-${resource}`] } }).catch(() => undefined);
  });
  const screenshots: string[] = [];
  const geometryFailures: string[] = [];
  const screenshot = (state: string, assertions: string[] = []) => capture(page, 'concurrent-page', state, 'en', viewport, screenshots, assertions, geometryFailures);
  try {
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/resource-loading.html?mode=unified&secondary-page=1`);
    const announcement = page.locator('[data-page-loading-announcement]');
    await announcement.getByText('Loading…', { exact: true }).waitFor();
    await page.evaluate(async () => { await document.fonts.ready; });
    assert.equal(await page.getByRole('status').count(), 1);
    const initial = await layout(page);
    await screenshot('initial-loading', ['one-status-for-multiple-page-reads']);
    gates.primary.release();
    await page.locator('[data-ready-resource="primary"]').waitFor();
    assert.equal(await announcement.textContent(), 'Loading…', 'the second page-level read retains the single pending announcement');
    assert.equal(await page.getByText('Loading…', { exact: true }).count(), 1);
    await page.locator('[data-fixture-action="available"]').click();
    assert.equal(await page.locator('[data-action-count]').textContent(), '1', 'page pending aggregation is not a request/action gate');
    assertStable(initial, await layout(page), 'one page-level read ready');
    await screenshot('one-page-ready', ['one-status-while-second-page-read-pending', 'completed-resource-usable', 'anchors-stable-within-1px']);
    gates.secondary.release();
    await page.locator('[data-ready-resource="secondary"]').waitFor();
    await page.waitForFunction(() => document.querySelector('[data-page-loading-announcement]')?.textContent === '');
    assert.equal(await page.locator('.mtc-loading-state').count(), 1, 'the optional history remains local after both page prerequisites finish');
    assertStable(initial, await layout(page), 'only optional section pending');
    await screenshot('local-only-slow', ['optional-read-no-page-status', 'anchors-stable-within-1px']);
    gates.tertiary.release();
    await page.locator('[data-ready-resource="tertiary"]').waitFor();
    assertStable(initial, await layout(page), 'all concurrent reads ready');
    await screenshot('ready', ['all-reads-completed', 'anchors-stable-within-1px']);
    assert.deepEqual(errors, []);
    assert.deepEqual(geometryFailures, []);
    await writeFile(`${artifacts}/concurrent-page--en--390x844--proof.json`, JSON.stringify({ status: 'PASS', coordinates: initial, screenshots, errors, geometryFailures }, null, 2));
  } catch (reason) {
    await screenshot('failure-evidence').catch(() => undefined);
    await writeFile(`${artifacts}/concurrent-page--en--390x844--proof.json`, JSON.stringify({ status: 'FAIL', screenshots, errors, geometryFailures, failure: reason instanceof Error ? reason.message : String(reason) }, null, 2));
    throw reason;
  } finally {
    Object.values(gates).forEach((gate) => gate.release());
    await browser.close();
    await server.close();
  }
});
