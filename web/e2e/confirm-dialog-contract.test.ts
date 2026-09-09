import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { readFile, readdir } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

// The contract typecheck intentionally excludes TSX browser fixtures.
declare global {
  interface Window {
    confirmFixture: {
      accepted: number;
      changeScope: () => void;
      duplicate: () => void;
      stale: () => void;
      unmount: () => void;
    };
  }
}

test('all destructive confirmation entry points use the scoped application dialog', async () => {
  for (const file of ['operator/GroupManager.tsx', 'operator/GenerationWorkspace.tsx', 'self/GenerationsPage.tsx', 'operator/pages/ManagementPages.tsx']) {
    const source = await readFile(new URL(`../src/${file}`, import.meta.url), 'utf8');
    assert.doesNotMatch(source, /window\.confirm/);
    assert.match(source, /await confirm\(/);
    assert.match(source, /\{confirmationDialog\}/);
  }
});

test('confirmation dialog is themed, keyboard-safe, single-flight and scope fenced', { timeout: 30_000 }, async (context) => {
  let executablePath = chromium.executablePath();
  if (!existsSync(executablePath)) {
    const cache = fileURLToPath(new URL('../../../../.cache/ms-playwright', import.meta.url));
    const entries = await readdir(cache, { withFileTypes: true }).catch(() => []);
    executablePath = entries.filter((entry) => entry.isDirectory() && entry.name.startsWith('chromium-'))
      .map((entry) => join(cache, entry.name, 'chrome-linux64', 'chrome')).find(existsSync) ?? '';
  }
  if (!executablePath) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required for confirmation dialog acceptance');
    context.skip('Chromium is not installed');
    return;
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ executablePath, headless: true });
  try {
    for (const theme of ['dark', 'light']) {
      const page = await browser.newPage({ viewport: { width: 320, height: 700 } });
      await page.addInitScript((value) => {
        localStorage.setItem('mtc-locale', 'en');
        document.addEventListener('DOMContentLoaded', () => { document.documentElement.dataset.theme = value; });
      }, theme);
      await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/confirm-dialog.html`);
      const trigger = page.getByRole('button', { name: 'Open confirmation', exact: true });
      const dialog = page.getByRole('dialog');
      const cancel = page.getByRole('button', { name: 'Cancel', exact: true });
      await trigger.click();
      await dialog.waitFor();
      assert.equal(await cancel.evaluate((element) => element === document.activeElement), true);
      assert.equal(await dialog.evaluate((element) => getComputedStyle(element).backgroundColor), theme === 'light' ? 'rgb(255, 255, 255)' : 'rgb(11, 25, 29)');
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
      await page.keyboard.press('Shift+Tab');
      assert.equal(await dialog.evaluate((element) => element.contains(document.activeElement)), true, 'keyboard focus remains inside the modal');
      await page.keyboard.press('Escape');
      await dialog.waitFor({ state: 'detached' });
      assert.equal(await trigger.evaluate((element) => element === document.activeElement), true);
      await trigger.click();
      await cancel.click();
      assert.equal(await page.evaluate(() => window.confirmFixture.accepted), 0);
      await page.evaluate(() => window.confirmFixture.duplicate());
      await page.getByRole('button', { name: 'Confirm and continue', exact: true }).click();
      await page.waitForFunction(() => window.confirmFixture.accepted === 1);
      await trigger.click();
      await page.evaluate(() => window.confirmFixture.changeScope());
      await dialog.waitFor({ state: 'detached' });
      await page.evaluate(() => window.confirmFixture.stale());
      assert.equal(await dialog.count(), 0, 'an old preflight closure cannot ask under new authority');
      assert.equal(await page.evaluate(() => window.confirmFixture.accepted), 1);
      await trigger.click();
      await dialog.waitFor();
      await page.evaluate(() => window.confirmFixture.unmount());
      await dialog.waitFor({ state: 'detached' });
      assert.equal(await page.evaluate(() => window.confirmFixture.accepted), 1);
      await page.close();
    }
  } finally {
    await browser.close();
    await server.close();
  }
});
