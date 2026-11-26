import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { hasDefaultChromium } from './support/browser-executable.js';
import { createServer } from 'vite';

declare global {
  interface Window {
    streamFetches: number;
    streamAborts: number;
    streamUrls: string[];
    emitStreamEvent: (eventId: string, eventAt: number) => void;
  }
}

test('the real SSE hook aborts while hidden, resumes from its cursor, and does not replay the cursor event', { timeout: 45_000 }, async (context) => {
  if (!hasDefaultChromium(context)) return;
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/request-event-stream.html`);
    await page.waitForFunction(() => window.streamFetches === 1);
    await page.getByTestId('stream-state').filter({ hasText: 'live' }).waitFor();
    await page.evaluate(() => window.emitStreamEvent('event-1', 100));
    await page.getByTestId('stream-events').filter({ hasText: 'event-1' }).waitFor();

    await page.evaluate(() => {
      Object.defineProperty(document, 'hidden', { configurable: true, value: true });
      document.dispatchEvent(new Event('visibilitychange'));
    });
    await page.waitForFunction(() => window.streamAborts === 1);

    await page.evaluate(() => {
      Object.defineProperty(document, 'hidden', { configurable: true, value: false });
      document.dispatchEvent(new Event('visibilitychange'));
    });
    await page.waitForFunction(() => window.streamFetches === 2);
    assert.match(await page.evaluate(() => window.streamUrls.at(-1) ?? ''), /after_event_at=100.*after_event_id=event-1/,
      'the restored stream asks for data strictly after the confirmed cursor');
    await page.getByTestId('stream-state').filter({ hasText: 'live' }).waitFor();

    await page.evaluate(() => window.emitStreamEvent('event-1', 100));
    await page.evaluate(() => window.emitStreamEvent('event-2', 101));
    await page.getByTestId('stream-events').filter({ hasText: 'event-1,event-2' }).waitFor();
    assert.equal(await page.getByTestId('stream-events').textContent(), 'event-1,event-2',
      'the cursor event itself cannot be replayed after a hidden-page reconnect');
  } finally { await browser.close(); await server.close(); }
});
