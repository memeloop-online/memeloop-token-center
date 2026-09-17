import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

declare global {
  interface Window {
    sessionDetailReads: number;
    sessionListReads: number;
    sessionListAborts: number;
    resolveSessionList: (status: boolean | number) => void;
    resolveSessionDetail: (status: boolean | number) => void;
  }
}

async function settleRenderedRefresh(page: import('playwright').Page, expectedListReads: number) {
  await page.waitForFunction((expected) => window.sessionListReads === expected, expectedListReads);
  await page.locator('.session-live-state.live').waitFor();
  // Two animation frames form a deterministic browser render boundary: the
  // refresh promise and any detail request it starts have both left the
  // microtask queue before the assertion reads the fixture counters.
  await page.evaluate(() => new Promise<void>((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
}

test('session reads retry a transient failure inside one deadline and remain cancellable', { timeout: 45_000 }, async () => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    return test.skip('Chromium required');
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/session-loading.html`);
    await page.waitForFunction(() => window.sessionListReads === 1);
    await page.locator('.session-list-skeleton').waitFor();
    assert.equal(await page.getByText('Retained session', { exact: true }).count(), 0);
    await page.evaluate(() => window.resolveSessionList(false));
    await page.waitForFunction(() => window.sessionListReads === 2);
    assert.equal(await page.getByRole('button', { name: 'Retry loading sessions', exact: true }).count(), 0,
      'an early 503 is retried before surfacing an error');
    await page.locator('.session-list-skeleton').waitFor();
    await page.evaluate(() => window.resolveSessionList(true));
    await page.getByText('Retained session', { exact: true }).first().waitFor();
    await page.getByRole('button', { name: 'Agent and request timing', exact: true }).click();
    assert.equal(await page.locator('.session-event footer > span').innerText(), '5 tokens · 10 ms · —',
      'unknown session cost renders as a dash instead of the recorded ledger amount');
    assert.equal(await page.getByRole('checkbox', { name: 'Auto-refresh', exact: true }).isChecked(), true);
    await page.getByRole('button', { name: 'Simulate session event', exact: true }).click();
    await page.waitForFunction(() => window.sessionListReads === 3);
    assert.ok(await page.getByText('Retained session', { exact: true }).count() > 0);
    await page.evaluate(() => window.resolveSessionList(false));
    await page.waitForFunction(() => window.sessionListReads === 4);
    assert.ok(await page.getByText('Retained session', { exact: true }).count() > 0, 'automatic retry retains the loaded page while pending');
    await page.getByRole('button', { name: 'Cancel', exact: true }).click();
    await page.waitForFunction(() => window.sessionListAborts === 1);
    assert.ok(await page.getByText('Retained session', { exact: true }).count() > 0, 'cancelling a retry does not blank the loaded page');
    await page.getByRole('button', { name: 'Refresh', exact: true }).click();
    await page.waitForFunction(() => window.sessionListReads === 5);
    await page.evaluate(() => window.resolveSessionList(true));
    const credential = page.locator('.session-controls input[role="combobox"]');
    const credentialId = '019f4b00-1111-7111-8111-111111111111';
    await credential.fill(credentialId);
    assert.equal(await credential.inputValue(), credentialId, 'a pasted real-shaped credential ID stays editable');
    await page.getByRole('button', { name: 'Apply filters', exact: true }).click();
    await page.waitForFunction(() => window.sessionListReads === 6);
    await page.evaluate(() => window.resolveSessionList(true));
    await page.getByText('Retained session', { exact: true }).first().waitFor();
    await credential.fill('alias-that-has-not-been-selected');
    assert.equal(await credential.inputValue(), 'alias-that-has-not-been-selected', 'editing must not silently restore the selected alias');
    assert.equal(await credential.evaluate(input => (input as HTMLInputElement).checkValidity()), false);
    await page.keyboard.press('Escape');
    await page.getByRole('button', { name: 'Clear filters', exact: true }).click();
    assert.equal(await credential.evaluate(input => (input as HTMLInputElement).checkValidity()), true, 'authoritative clearing removes stale custom validity');
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/session-loading.html?titles=1`);
    await page.waitForFunction(() => window.sessionListReads === 1);
    await page.evaluate(() => window.resolveSessionList(true));
    await page.getByRole('heading', { name: 'Current context name', exact: true }).waitFor();
    assert.match(await page.locator('.session-sidebar-item[aria-pressed="true"]').getAttribute('aria-label') ?? '', /Fixture key/);
    assert.doesNotMatch(await page.locator('.session-sidebar-item[aria-pressed="true"]').getAttribute('aria-label') ?? '', /fixture-session/);
  } finally { await browser.close(); await server.close(); }
});

test('session events refresh only the exact selected credential and session detail', { timeout: 45_000 }, async () => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    return test.skip('Chromium required');
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/session-loading.html`);
    await page.waitForFunction(() => window.sessionListReads === 1);
    await page.evaluate(() => window.resolveSessionList(true));
    await page.waitForFunction(() => window.sessionDetailReads === 1);

    // Queue an exact-selection event without a revision, then replace the
    // filter projection. Its authoritative snapshot must consume the old
    // scope boundary so the next unrelated revision cannot drain both events.
    await page.getByRole('button', { name: 'Queue stale session event', exact: true }).click();
    await page.locator('.session-controls').getByLabel('Model', { exact: true }).fill('new-projection');
    await page.getByRole('button', { name: 'Apply filters', exact: true }).click();
    await page.waitForFunction(() => window.sessionListReads === 2);
    await page.evaluate(() => window.resolveSessionList(true));
    await page.waitForFunction(() => window.sessionDetailReads === 2);

    await page.getByRole('button', { name: 'Simulate other credential event', exact: true }).click();
    await page.waitForFunction(() => window.sessionListReads === 3);
    await page.evaluate(() => window.resolveSessionList(true));
    await settleRenderedRefresh(page, 3);
    assert.equal(await page.evaluate(() => window.sessionDetailReads), 2,
      'another credential may update the tenant list but must not drain a stale selected-detail event');

    await page.getByRole('button', { name: 'Simulate other session event', exact: true }).click();
    await page.waitForFunction(() => window.sessionListReads === 4);
    await page.evaluate(() => window.resolveSessionList(true));
    await settleRenderedRefresh(page, 4);
    assert.equal(await page.evaluate(() => window.sessionDetailReads), 2,
      'another session on the same credential must not refresh the selected detail');

    await page.getByRole('button', { name: 'Simulate archive bound event', exact: true }).click();
    await page.waitForFunction(() => window.sessionListReads === 5);
    await page.evaluate(() => window.resolveSessionList(true));
    await page.waitForFunction(() => window.sessionDetailReads === 3);
    assert.equal(await page.evaluate(() => window.sessionDetailReads), 3,
      'archive_bound refreshes the exact selected request although HTTP status and combined archive state stay unchanged');

    await page.getByRole('button', { name: 'Simulate session event', exact: true }).click();
    await page.waitForFunction(() => window.sessionListReads === 6);
    await page.evaluate(() => window.resolveSessionList(true));
    await page.waitForFunction(() => window.sessionDetailReads === 4);
    assert.equal(await page.evaluate(() => window.sessionDetailReads), 4,
      'the exact selected credential and session event refreshes its detail once');

    await page.getByRole('button', { name: 'Open Unlinked requests', exact: true }).click();
    await page.waitForFunction(() => window.sessionDetailReads === 5);
    await page.getByRole('button', { name: 'Simulate confirmed projection', exact: true }).click();
    await page.waitForFunction(() => window.sessionListReads === 7);
    await page.evaluate(() => window.resolveSessionList(true));
    await page.waitForFunction(() => window.sessionDetailReads === 6);
    assert.equal(await page.evaluate(() => window.sessionDetailReads), 6,
      'a confirmed projection refreshes the old same-credential unlinked detail');
  } finally { await browser.close(); await server.close(); }
});

test('manual and background pauses resume with authoritative list and selected detail reads', { timeout: 45_000 }, async () => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    return test.skip('Chromium required');
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/session-loading.html`);
    await page.waitForFunction(() => window.sessionListReads === 1);
    await page.evaluate(() => window.resolveSessionList(true));
    await page.waitForFunction(() => window.sessionDetailReads === 1);

    const autoRefresh = page.getByRole('checkbox', { name: 'Auto-refresh', exact: true });
    await autoRefresh.uncheck();
    await page.getByRole('button', { name: 'Simulate session event', exact: true }).click();
    await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
    assert.equal(await page.evaluate(() => window.sessionListReads), 1, 'manual pause keeps queued events without reading');
    await autoRefresh.check();
    await page.waitForFunction(() => window.sessionListReads === 2);
    await page.evaluate(() => window.resolveSessionList(true));
    await page.waitForFunction(() => window.sessionDetailReads === 2);

    await page.getByRole('button', { name: 'Pause shared refresh', exact: true }).click();
    await page.getByRole('button', { name: 'Simulate session event', exact: true }).click();
    await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
    assert.equal(await page.evaluate(() => window.sessionListReads), 2, 'background pause keeps queued events without reading');
    await page.getByRole('button', { name: 'Resume shared refresh', exact: true }).click();
    await page.waitForFunction(() => window.sessionListReads === 3);
    await page.evaluate(() => window.resolveSessionList(true));
    await page.waitForFunction(() => window.sessionDetailReads === 3);

    await page.getByRole('button', { name: 'Simulate session event overflow', exact: true }).click();
    await page.waitForFunction(() => window.sessionListReads === 4);
    await page.evaluate(() => window.resolveSessionList(true));
    await page.waitForFunction(() => window.sessionDetailReads === 4);
  } finally { await browser.close(); await server.close(); }
});

test('a failed SSE list batch is retained and retried on the next session cadence', { timeout: 45_000 }, async () => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    return test.skip('Chromium required');
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    await page.clock.install({ time: new Date('2026-01-01T00:00:00Z') });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/session-loading.html`);
    await page.waitForFunction(() => window.sessionListReads === 1);
    await page.evaluate(() => window.resolveSessionList(true));
    await page.waitForFunction(() => window.sessionDetailReads === 1);
    await page.getByRole('button', { name: 'Simulate session event', exact: true }).click();
    await page.clock.fastForward(500);
    await page.waitForFunction(() => window.sessionListReads === 2);
    // 400 is deliberately non-retryable inside apiRead. The only way read 3
    // can occur is if SessionMonitor restores this drained SSE batch.
    await page.evaluate(() => window.resolveSessionList(400));
    await page.getByRole('alert').waitFor();
    await page.clock.fastForward(500);
    await page.waitForFunction(() => window.sessionListReads === 3);
    await page.evaluate(() => window.resolveSessionList(true));
  } finally { await browser.close(); await server.close(); }
});

test('a failed SSE detail batch is retained after the detail lane settles', { timeout: 45_000 }, async () => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    return test.skip('Chromium required');
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    await page.clock.install({ time: new Date('2026-01-01T00:00:00Z') });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/session-loading.html?controlled-detail=1`);
    await page.waitForFunction(() => window.sessionListReads === 1);
    await page.evaluate(() => window.resolveSessionList(true));
    await page.waitForFunction(() => window.sessionDetailReads === 1);
    await page.evaluate(() => window.resolveSessionDetail(true));
    await page.getByRole('button', { name: 'Simulate session event', exact: true }).click();
    await page.clock.fastForward(500);
    await page.waitForFunction(() => window.sessionListReads === 2);
    await page.evaluate(() => window.resolveSessionList(true));
    await page.waitForFunction(() => window.sessionDetailReads === 2);
    // As above, the non-retryable detail failure forces the re-arm to be
    // owned by SessionMonitor rather than apiRead's retry policy.
    await page.evaluate(() => window.resolveSessionDetail(400));
    await page.getByRole('alert').waitFor();
    await page.clock.fastForward(500);
    await page.waitForFunction(() => window.sessionListReads === 3);
    await page.evaluate(() => window.resolveSessionList(true));
    await page.waitForFunction(() => window.sessionDetailReads === 3);
    await page.evaluate(() => window.resolveSessionDetail(true));
  } finally { await browser.close(); await server.close(); }
});

test('cancellation retains current-scope SSE work, while a scope transition drops stale work', { timeout: 45_000 }, async () => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    return test.skip('Chromium required');
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    await page.clock.install({ time: new Date('2026-01-01T00:00:00Z') });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/session-loading.html`);
    await page.waitForFunction(() => window.sessionListReads === 1);
    await page.evaluate(() => window.resolveSessionList(true));
    await page.waitForFunction(() => window.sessionDetailReads === 1);
    await page.getByRole('button', { name: 'Simulate session event', exact: true }).click();
    await page.clock.fastForward(500);
    await page.waitForFunction(() => window.sessionListReads === 2);
    await page.getByRole('button', { name: 'Cancel', exact: true }).click();
    await page.waitForFunction(() => window.sessionListAborts === 1);
    // Cancellation retains the batch but must not create an unsolicited new
    // request; the user's next refresh owns the same current-scope work.
    await page.clock.fastForward(1_000);
    assert.equal(await page.evaluate(() => window.sessionListReads), 2);
    await page.getByRole('button', { name: 'Refresh', exact: true }).click();
    await page.waitForFunction(() => window.sessionListReads === 3);
    await page.getByRole('button', { name: 'Cancel', exact: true }).click();
    await page.waitForFunction(() => window.sessionListAborts === 2);
    await page.clock.fastForward(1_000);
    assert.equal(await page.evaluate(() => window.sessionListReads), 3,
      'cancelling a manual refresh must not let its finally block restart the retained batch');

    await page.getByRole('button', { name: 'Simulate other credential event', exact: true }).click();
    await page.clock.fastForward(500);
    await page.waitForFunction(() => window.sessionListReads === 4);
    await page.evaluate(() => window.resolveSessionList(true));
    await page.clock.fastForward(1_000);
    assert.equal(await page.evaluate(() => window.sessionListReads), 4,
      'the next real SSE clears cancellation once and schedules exactly one refresh');

    await page.getByRole('button', { name: 'Queue stale session event', exact: true }).click();
    await page.locator('.session-controls').getByLabel('Model', { exact: true }).fill('new-projection');
    await page.getByRole('button', { name: 'Apply filters', exact: true }).click();
    await page.waitForFunction(() => window.sessionListReads === 5);
    await page.evaluate(() => window.resolveSessionList(true));
    await page.clock.fastForward(1_000);
    assert.equal(await page.evaluate(() => window.sessionListReads), 5,
      'the queued old-projection event cannot run after the new scope snapshot');
  } finally { await browser.close(); await server.close(); }
});

test('a successful manual refresh confirms a cancelled selected-detail batch before later unrelated SSE work', { timeout: 45_000 }, async () => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    return test.skip('Chromium required');
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    await page.clock.install({ time: new Date('2026-01-01T00:00:00Z') });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/session-loading.html?controlled-detail=1`);
    await page.waitForFunction(() => window.sessionListReads === 1);
    await page.evaluate(() => window.resolveSessionList(true));
    await page.waitForFunction(() => window.sessionDetailReads === 1);
    await page.evaluate(() => window.resolveSessionDetail(true));
    await page.getByRole('button', { name: 'Simulate session event', exact: true }).click();
    await page.clock.fastForward(500);
    await page.waitForFunction(() => window.sessionListReads === 2);
    await page.getByRole('button', { name: 'Cancel', exact: true }).click();
    await page.waitForFunction(() => window.sessionListAborts === 1);

    // The user explicitly observes both lanes.  That confirmation must
    // consume the cancelled batch rather than merely suppress its timer.
    await page.getByRole('button', { name: 'Refresh', exact: true }).click();
    await page.waitForFunction(() => window.sessionListReads === 3);
    await page.evaluate(() => window.resolveSessionList(true));
    await page.waitForFunction(() => window.sessionDetailReads === 2);
    await page.evaluate(() => window.resolveSessionDetail(true));
    await settleRenderedRefresh(page, 3);

    await page.getByRole('button', { name: 'Simulate other credential event', exact: true }).click();
    await page.clock.fastForward(500);
    await page.waitForFunction(() => window.sessionListReads === 4);
    await page.evaluate(() => window.resolveSessionList(true));
    await settleRenderedRefresh(page, 4);
    assert.equal(await page.evaluate(() => window.sessionDetailReads), 2,
      'the unrelated event refreshes its tenant list only; it cannot replay the selected detail already confirmed manually');
  } finally { await browser.close(); await server.close(); }
});
