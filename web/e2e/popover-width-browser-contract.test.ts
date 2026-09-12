import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

test('popover reopening and viewport resizing never expose old-width coordinates', { timeout: 30_000 }, async (t) => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required for popover geometry contracts');
    t.skip('Chromium is not installed'); return;
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 320, height: 600 } });
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/popover-width.html`);
    await page.locator('#trigger').waitFor();
    for (const width of [320, 390, 1024, 390, 320]) {
      await page.setViewportSize({ width, height: 600 });
      // Sample the first layout frame, before ResizeObserver could repair an
      // incorrectly measured initial placement. No timed wait or boundary polling.
      const firstFrame = await page.evaluate(async () => {
        document.querySelector<HTMLButtonElement>('#trigger')!.click();
        await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
        const panel = document.querySelector<HTMLElement>('#panel')!;
        const bounds = panel.getBoundingClientRect();
        return { left: bounds.left, right: bounds.right, width: bounds.width, viewport: innerWidth, maxWidth: panel.style.maxWidth };
      });
      assert.ok(firstFrame.left >= 0 && firstFrame.right <= width, JSON.stringify({ phase: 'reopen', firstFrame }));
      assert.equal(firstFrame.width, width - 16, JSON.stringify(firstFrame));
      await page.locator('#trigger').click();
      await page.locator('#panel').waitFor({ state: 'detached' });
    }
    await page.locator('#trigger').click();
    for (const width of [1024, 390, 320, 768]) {
      // Record every rendered frame across live resizing, not just a settled
      // final rectangle that could conceal a visible one-frame overflow.
      await page.evaluate(() => {
        const state = { frames: [] as Array<{ left: number; right: number; viewport: number }>, active: true };
        Object.assign(window, { popoverGeometry: state });
        const sample = () => {
          if (!state.active) return;
          const bounds = document.querySelector('#panel')!.getBoundingClientRect();
          state.frames.push({ left: bounds.left, right: bounds.right, viewport: innerWidth });
          requestAnimationFrame(sample);
        };
        requestAnimationFrame(sample);
      });
      await page.setViewportSize({ width, height: 600 });
      const frames = await page.evaluate(async () => {
        await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
        const state = (window as unknown as { popoverGeometry: { active: boolean; frames: Array<{ left: number; right: number; viewport: number }> } }).popoverGeometry;
        state.active = false;
        return state.frames;
      });
      assert.ok(frames.some((frame) => frame.viewport === width), JSON.stringify({ width, frames }));
      for (const frame of frames) assert.ok(frame.left >= 0 && frame.right <= frame.viewport, JSON.stringify({ phase: 'resize', frame }));
    }
  } finally { await browser.close(); await server.close(); }
});
