import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';
import { localChromiumExecutable } from './support/browser-executable.js';
import type {} from './fixtures/operator-login-cycle.js';

test('remembered login ownership fences delayed cross-tab events at request start, response and clear', { timeout: 120_000 }, async context => {
  const executablePath = await localChromiumExecutable();
  if (!executablePath) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required for remembered login ownership');
    return context.skip('Chromium runtime required');
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ executablePath, headless: true });
  async function readyPage() {
    const page = await browser.newPage();
    await page.addInitScript(() => {
      localStorage.setItem('mtc.operator.service-credential.v1', 'synthetic-identity-A');
      localStorage.setItem('mtc.operator.login-cycle.v1', 'synthetic-cycle-A');
    });
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/operator-login-cycle.html`);
    await page.waitForFunction(() => window.loginCycleFixture.state.status === 'ready');
    assert.equal(await page.evaluate(() => window.loginCycleFixture.state.cycle), 'synthetic-cycle-A');
    return page;
  }
  async function assertOtherIdentity(page: Awaited<ReturnType<typeof readyPage>>) {
    await page.waitForFunction(() => window.loginCycleFixture.state.status === 'disconnected');
    assert.deepEqual(await page.evaluate(() => ({
      credential: localStorage.getItem('mtc.operator.service-credential.v1'),
      cycle: localStorage.getItem('mtc.operator.login-cycle.v1'),
      validated: window.loginCycleFixture.state.validated,
    })), { credential: 'synthetic-identity-B', cycle: 'synthetic-cycle-B', validated: false });
  }
  try {
    for (const action of ['refresh', 'revalidate', 'replacement', 'clear'] as const) {
      const page = await readyPage();
      const before = await page.evaluate(() => window.loginCycleFixture.calls);
      await page.evaluate(action => {
        window.loginCycleFixture.replaceStorage();
        window.loginCycleFixture.invoke(action);
      }, action);
      await assertOtherIdentity(page);
      assert.equal(await page.evaluate(() => window.loginCycleFixture.calls), before, `${action}: stale origin must be rejected before tenant discovery`);
      await page.close();
    }
    for (const action of ['refresh', 'revalidate', 'replacement'] as const) {
      for (const status of [200, 401, 403]) {
        const page = await readyPage();
        const before = await page.evaluate(() => window.loginCycleFixture.calls);
        await page.evaluate(action => {
          window.loginCycleFixture.holdNext = true;
          window.loginCycleFixture.invoke(action);
        }, action);
        await page.waitForFunction(count => window.loginCycleFixture.calls === count, before + 1);
        await page.evaluate(status => {
          window.loginCycleFixture.replaceStorage();
          window.loginCycleFixture.release(status);
        }, status);
        await assertOtherIdentity(page);
        assert.equal(await page.evaluate(() => window.loginCycleFixture.calls), before + 1, `${action}/${status}: no replacement discovery after stale completion`);
        await page.close();
      }
    }
    for (const status of [401, 403]) {
      const disconnected = await readyPage();
      await disconnected.evaluate(() => {
        window.loginCycleFixture.replaceStorage();
        window.loginCycleFixture.invoke('clear');
      });
      await assertOtherIdentity(disconnected);
      await disconnected.evaluate(() => {
        window.loginCycleFixture.holdNext = true;
        window.loginCycleFixture.invoke('replacement');
      });
      await disconnected.waitForFunction(() => window.loginCycleFixture.calls === 2);
      await disconnected.evaluate(status => window.loginCycleFixture.release(status), status);
      await disconnected.waitForFunction(() => window.loginCycleFixture.state.status === 'failed');
      assert.deepEqual(await disconnected.evaluate(() => ({
        credential: localStorage.getItem('mtc.operator.service-credential.v1'),
        cycle: localStorage.getItem('mtc.operator.login-cycle.v1'),
        validated: window.loginCycleFixture.state.validated,
      })), { credential: 'synthetic-identity-B', cycle: 'synthetic-cycle-B', validated: false }, `disconnected candidate C/${status}: B remains owned by the other tab`);
      await disconnected.close();
    }
    const page = await readyPage();
    await page.evaluate(() => {
      window.loginCycleFixture.holdNext = true;
      window.loginCycleFixture.invoke('replacement');
    });
    await page.waitForFunction(() => window.loginCycleFixture.calls === 2);
    await page.evaluate(() => window.loginCycleFixture.release(401));
    await page.waitForFunction(() => window.loginCycleFixture.state.status === 'failed');
    assert.deepEqual(await page.evaluate(() => ({ credential: localStorage.getItem('mtc.operator.service-credential.v1'), cycle: localStorage.getItem('mtc.operator.login-cycle.v1'), validated: window.loginCycleFixture.state.validated })), { credential: 'synthetic-identity-A', cycle: 'synthetic-cycle-A', validated: true });
    await page.evaluate(() => window.loginCycleFixture.invoke('clear'));
    await page.waitForFunction(() => window.loginCycleFixture.state.status === 'disconnected');
    assert.deepEqual(await page.evaluate(() => [localStorage.getItem('mtc.operator.service-credential.v1'), localStorage.getItem('mtc.operator.login-cycle.v1')]), [null, null]);
    await page.close();
    const blocked = await browser.newPage();
    await blocked.addInitScript(() => {
      const original = Storage.prototype.setItem;
      Storage.prototype.setItem = function(key, value) {
        if (key.startsWith('mtc.operator.')) throw new DOMException('Synthetic blocked storage', 'SecurityError');
        return original.call(this, key, value);
      };
    });
    await blocked.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/operator-login-cycle.html`);
    await blocked.waitForFunction(() => window.loginCycleFixture.state.status === 'disconnected');
    await blocked.evaluate(() => window.loginCycleFixture.invoke('connect'));
    await blocked.waitForFunction(() => window.loginCycleFixture.state.status === 'ready');
    assert.equal(await blocked.evaluate(() => window.loginCycleFixture.state.cycle), undefined);
    for (const action of ['refresh', 'revalidate'] as const) {
      const calls = await blocked.evaluate(() => window.loginCycleFixture.calls);
      await blocked.evaluate(action => window.loginCycleFixture.invoke(action), action);
      await blocked.waitForFunction(count => window.loginCycleFixture.calls === count && window.loginCycleFixture.state.status === 'ready', calls + 1);
      assert.equal(await blocked.evaluate(() => window.loginCycleFixture.state.validated), true, `${action}: explicit in-memory login survives blocked storage`);
    }
    await blocked.close();
  } finally { await browser.close(); await server.close(); }
});
