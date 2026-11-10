import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

declare global {
  interface Window {
    requestNavigationFixture: {
      queryCalls: number;
      detailCalls: number;
      upstreamCalls: number;
      streamCalls: number;
      queryHeld: boolean;
      detailHeld: boolean;
      streamOpen: boolean;
      emit: (kind: 'started' | 'finished') => void;
      releaseQuery: () => void;
      releaseDetail: () => void;
    };
    requestNavigationMutations?: string[];
  }
}

test('held REST gates cached unlinked SSE and tab remounts keep confirmed session metadata', { timeout: 45_000 }, async (context) => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required');
    context.skip('Chromium is not installed');
    return;
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const origin = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    await page.route('**/*', route => {
      const url = new URL(route.request().url());
      return url.origin === origin && !url.pathname.startsWith('/internal/') ? route.continue() : route.abort();
    });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'zh-CN'));
    await page.goto(`${origin}/e2e/fixtures/request-navigation.html`);

    const revision = () => page.evaluate(() => Number(document.querySelector('main')?.getAttribute('data-stream-revision') ?? '0'));
    const waitForPublish = (before: number) => page.waitForFunction(
      (baseline) => Number(document.querySelector('main')?.getAttribute('data-stream-revision') ?? '0') > baseline,
      before,
    );

    await page.waitForFunction(() => window.requestNavigationFixture?.streamOpen === true);
    await page.waitForFunction(() => window.requestNavigationFixture?.queryHeld === true);

    const baseline = await revision();
    await page.evaluate(() => window.requestNavigationFixture.emit('started'));
    await waitForPublish(baseline);
    // Flush passive effects so a leaky loading gate would already have rendered.
    await page.evaluate(() => new Promise<void>((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));

    assert.equal(await page.locator('tbody tr').count(), 0);
    assert.equal(await page.locator('.request-session-unlinked').count(), 0);
    assert.equal(await page.getByText('model-live', { exact: true }).count(), 0);
    assert.equal(await page.locator('.request-page-surface .mtc-loading-state').count(), 1);

    await page.evaluate(() => window.requestNavigationFixture.releaseQuery());
    const row = page.locator('tbody tr').filter({ hasText: 'model-live' });
    await row.waitFor();
    await row.locator('.request-session-cell').filter({ hasText: '深夜调研会话' }).waitFor();
    assert.equal(await row.locator('.request-session-unlinked').count(), 0);
    assert.equal(await page.evaluate(() => window.requestNavigationFixture.queryCalls), 1);

    await row.locator('.table-action').click();
    await page.waitForFunction(() => window.requestNavigationFixture.detailHeld === true);
    const heldCellText = await row.locator('.request-session-cell').textContent() ?? '';
    assert.match(heldCellText, /深夜调研会话/);
    assert.ok(!heldCellText.includes('未关联请求'), 'a delayed detail cannot drop confirmed session metadata');
    assert.equal(await page.getByRole('alert').count(), 0);
    assert.equal(await page.evaluate(() => window.requestNavigationFixture.queryCalls), 1, 'opening a detail never re-queries the list');
    await page.evaluate(() => window.requestNavigationFixture.releaseDetail());
    const drawer = page.getByRole('dialog', { name: 'model-live' });
    await drawer.waitFor();
    await drawer.getByText('深夜调研会话', { exact: true }).waitFor();
    assert.equal(await page.evaluate(() => window.requestNavigationFixture.detailCalls), 1);
    await drawer.getByRole('button', { name: '关闭', exact: true }).click();
    await drawer.waitFor({ state: 'hidden' });

    await page.getByTestId('tab-other').click();
    await page.getByTestId('other-panel').waitFor();
    assert.equal(await page.locator('tbody tr').count(), 0);
    const awayBaseline = await revision();
    await page.evaluate(() => window.requestNavigationFixture.emit('finished'));
    await waitForPublish(awayBaseline);
    await page.evaluate(() => new Promise<void>((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
    assert.equal(await page.evaluate(() => window.requestNavigationFixture.queryCalls), 1, 'an unmounted page never queries');

    await page.evaluate(() => {
      window.requestNavigationMutations = [];
      new MutationObserver(() => {
        const surface = document.querySelector('.request-page-surface');
        if (surface) window.requestNavigationMutations?.push(surface.textContent ?? '');
      }).observe(document.getElementById('root')!, { childList: true, subtree: true, characterData: true });
    });
    await page.getByTestId('tab-requests').click();
    await page.waitForFunction(() => window.requestNavigationFixture.queryHeld === true);
    assert.equal(await page.evaluate(() => window.requestNavigationFixture.queryCalls), 2, 'exactly one list query per mount');
    await page.evaluate(() => window.requestNavigationFixture.releaseQuery());
    const remounted = page.locator('tbody tr').filter({ hasText: 'model-live' });
    await remounted.waitFor();
    await remounted.locator('.request-session-cell').filter({ hasText: '深夜调研会话' }).waitFor();
    const snapshots = await page.evaluate(() => window.requestNavigationMutations ?? []);
    assert.ok(snapshots.length > 0, 'the observer recorded the remount rendering');
    for (const snapshot of snapshots) {
      assert.ok(!snapshot.includes('未关联请求'), 'remount must never show the unlinked label for a confirmed session');
    }
    assert.equal(await page.evaluate(() => window.requestNavigationFixture.detailCalls), 1, 'a remount does not refetch the detail');
    assert.equal(await page.evaluate(() => window.requestNavigationFixture.upstreamCalls), 2, 'one upstream read per mount');
    assert.equal(await page.evaluate(() => window.requestNavigationFixture.streamCalls), 1, 'the SSE stream survives tab switches');

    await page.getByTestId('tab-overview').click();
    await page.waitForFunction(() => !window.requestNavigationFixture.streamOpen);
    await page.getByTestId('tab-requests').click();
    await page.waitForFunction(() => window.requestNavigationFixture.queryHeld && window.requestNavigationFixture.streamOpen);
    assert.equal(await page.locator('tbody tr').count(), 0);
    assert.equal(await page.locator('.request-session-unlinked').count(), 0);
    await page.evaluate(() => window.requestNavigationFixture.releaseQuery());
    await page.locator('.request-session-cell').filter({ hasText: '深夜调研会话' }).waitFor();
    assert.equal(await page.evaluate(() => window.requestNavigationFixture.queryCalls), 3);
    assert.equal(await page.evaluate(() => window.requestNavigationFixture.streamCalls), 2);
    assert.equal(await page.evaluate(() => window.requestNavigationFixture.detailCalls), 1);
  } finally {
    await browser.close();
    await server.close();
  }
});
