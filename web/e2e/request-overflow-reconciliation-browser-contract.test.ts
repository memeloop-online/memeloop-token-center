import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

import {
  requestOverflowReconcileCooldownMs,
  requestOverflowReconcileDelayMs,
} from '../src/operator/traffic/requestOverflowReconciliation.js';

declare global {
  interface Window {
    requestQueryReads: number;
    emitRequestOverflow: (count?: number) => Promise<void>;
    emitLiveRequest: (id: string, createdAt: number) => void;
    resolveNextRequestQuery: (id: string) => void;
    requestQueryKinds: string[];
  }
}

test('Requests keeps realtime events flowing while overflow reconciliation coalesces behind a hard cooldown', { timeout: 45_000 }, async () => {
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
    const model = (value: string) => page.locator('.request-model-cell code').filter({ hasText: new RegExp(`^${value}$`) });
    await page.clock.install({ time: new Date('2026-01-01T00:00:00Z') });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/request-overflow-reconciliation.html`);
    await page.waitForFunction(() => window.requestQueryReads === 1);
    await model('initial-authoritative').waitFor();
    // install() virtualizes timers but intentionally lets time keep flowing.
    // Freeze only after boot so CI render time can never consume a contract
    // interval before an assertion explicitly advances it.
    await page.clock.pauseAt(new Date('2026-01-01T01:00:00Z'));

    await page.evaluate(() => window.emitRequestOverflow(1_000));
    await page.clock.fastForward(requestOverflowReconcileDelayMs - 1);
    assert.equal(await page.evaluate(() => window.requestQueryReads), 1);
    await page.clock.fastForward(1);
    assert.equal(await page.evaluate(() => window.requestQueryReads), 2);

    await page.evaluate(async () => {
      window.emitLiveRequest('live-during-reconcile', 2_000);
      await window.emitRequestOverflow(1_000);
    });
    await model('live-during-reconcile').waitFor();
    assert.equal(await page.evaluate(() => window.requestQueryReads), 2, 'in-flight overflow cannot fan out query POSTs');

    await page.evaluate(() => window.resolveNextRequestQuery('first-reconcile'));
    await model('first-reconcile').waitFor();
    await model('live-during-reconcile').waitFor();
    await page.clock.fastForward(requestOverflowReconcileCooldownMs - 1);
    assert.equal(await page.evaluate(() => window.requestQueryReads), 2);
    await page.clock.fastForward(1);
    assert.equal(await page.evaluate(() => window.requestQueryReads), 3);

    await page.evaluate(() => window.resolveNextRequestQuery('final-reconcile'));
    await model('final-reconcile').waitFor();
    await page.clock.fastForward(requestOverflowReconcileCooldownMs * 2);
    assert.equal(await page.evaluate(() => window.requestQueryReads), 3, 'one clean trailing pass ends the catch-up episode');
    await model('live-during-reconcile').waitFor();

    // Exercise RequestsPage.load(filters, true), not just the coordinator in
    // isolation: pagination aborts the active first-page query, loads history,
    // then restores the one sticky reconciliation it interrupted.
    await page.evaluate(() => window.emitRequestOverflow(1_000));
    await page.clock.fastForward(requestOverflowReconcileDelayMs);
    assert.equal(await page.evaluate(() => window.requestQueryReads), 4);
    await page.getByRole('button', { name: 'Load older requests', exact: true }).click();
    await model('older-page').waitFor();
    assert.equal(await page.evaluate(() => window.requestQueryReads), 5);
    assert.deepEqual(await page.evaluate(() => window.requestQueryKinds), ['first', 'first', 'first', 'first', 'older']);

    await page.clock.fastForward(requestOverflowReconcileDelayMs);
    assert.equal(await page.evaluate(() => window.requestQueryReads), 6);
    assert.deepEqual(await page.evaluate(() => window.requestQueryKinds), ['first', 'first', 'first', 'first', 'older', 'first'],
      'the interrupted sticky overflow is restored as a first-page query after pagination');
    await page.evaluate(() => window.resolveNextRequestQuery('post-pagination-reconcile'));
    await model('post-pagination-reconcile').waitFor();
    await model('older-page').waitFor();
  } finally {
    await browser.close();
    await server.close();
  }
});
