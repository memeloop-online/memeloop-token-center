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
    emitRequestOverflow: (count?: number) => void;
    emitLiveRequest: (id: string, createdAt: number) => void;
    resolveNextRequestQuery: (id: string) => void;
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

    await page.evaluate(() => window.emitRequestOverflow(1_000));
    await page.getByTestId('overflow-revision').filter({ hasText: '1000' }).waitFor({ state: 'attached' });
    await page.clock.fastForward(requestOverflowReconcileDelayMs - 1);
    assert.equal(await page.evaluate(() => window.requestQueryReads), 1);
    await page.clock.fastForward(1);
    await page.waitForFunction(() => window.requestQueryReads === 2);

    await page.evaluate(() => {
      window.emitLiveRequest('live-during-reconcile', 2_000);
      window.emitRequestOverflow(1_000);
    });
    await model('live-during-reconcile').waitFor();
    assert.equal(await page.evaluate(() => window.requestQueryReads), 2, 'in-flight overflow cannot fan out query POSTs');

    await page.evaluate(() => window.resolveNextRequestQuery('first-reconcile'));
    await model('first-reconcile').waitFor();
    await model('live-during-reconcile').waitFor();
    await page.clock.fastForward(requestOverflowReconcileCooldownMs - 1);
    assert.equal(await page.evaluate(() => window.requestQueryReads), 2);
    await page.clock.fastForward(1);
    await page.waitForFunction(() => window.requestQueryReads === 3);

    await page.evaluate(() => window.resolveNextRequestQuery('final-reconcile'));
    await model('final-reconcile').waitFor();
    await page.clock.fastForward(requestOverflowReconcileCooldownMs * 2);
    assert.equal(await page.evaluate(() => window.requestQueryReads), 3, 'one clean trailing pass ends the catch-up episode');
    await model('live-during-reconcile').waitFor();
  } finally {
    await browser.close();
    await server.close();
  }
});
