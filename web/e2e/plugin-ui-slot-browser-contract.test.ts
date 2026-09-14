import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { mkdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

test('plugin slots are lazy, scoped, isolated and safe on mobile in both themes', { timeout: 60_000 }, async () => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required');
    return test.skip('Chromium is required');
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 360, height: 740 } });
    const reads: string[] = [];
    const errors: string[] = [];
    page.on('pageerror', (error) => errors.push(error.message));
    await page.route('**/fixture/projection/*?*', async (route) => {
      const url = new URL(route.request().url());
      const plugin = url.pathname.split('/').at(-1)!;
      const tenant = url.searchParams.get('tenant');
      reads.push(`${plugin}:${tenant}`);
      await route.fulfill({ json: { schema_version: 1, plugin_id: plugin, slot_id: 'summary', components: plugin === 'broken'
        ? [{ kind: 'link', label: 'Unsafe', href: 'javascript:alert(1)' }]
        : [{ kind: 'text', text: `<img src=x onerror=alert(1)> ${tenant}` }, { kind: 'metric', label: 'Requests', value: '12' }, { kind: 'status', label: 'Service', state: 'ok' }, { kind: 'link', label: 'Documentation', href: 'https://example.com/docs' }] } });
    });
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/plugin-ui-slot.html`);
    const healthy = page.getByRole('region', { name: 'healthy', exact: true });
    const broken = page.getByRole('region', { name: 'broken', exact: true });
    await healthy.waitFor();
    assert.equal(reads.length, 0, 'offscreen slots do not fan out reads');
    await broken.scrollIntoViewIfNeeded();
    await healthy.getByText('<img src=x onerror=alert(1)> alpha', { exact: true }).waitFor();
    await broken.getByText('Plugin unavailable', { exact: true }).waitFor();
    assert.equal(await healthy.locator('img, script, iframe').count(), 0);
    assert.equal(await broken.getByRole('link').count(), 0);
    const link = healthy.getByRole('link', { name: 'Documentation' });
    assert.equal(await link.getAttribute('rel'), 'noopener noreferrer');
    assert.equal(await link.getAttribute('referrerpolicy'), 'no-referrer');
    await link.focus();
    assert.equal(await link.evaluate((element) => element === document.activeElement), true);
    await page.getByRole('button', { name: 'Switch tenant' }).click();
    assert.equal(await page.getByText('<img src=x onerror=alert(1)> alpha', { exact: true }).count(), 0);
    await broken.scrollIntoViewIfNeeded();
    await healthy.getByText('<img src=x onerror=alert(1)> beta', { exact: true }).waitFor();
    const evidence = fileURLToPath(new URL('../e2e-artifacts/plugin-ui-slots/', import.meta.url));
    await mkdir(evidence, { recursive: true });
    await page.screenshot({ path: `${evidence}/mobile-dark.png` });
    await page.getByRole('button', { name: 'Light theme' }).click();
    await broken.scrollIntoViewIfNeeded();
    await page.screenshot({ path: `${evidence}/mobile-light.png` });
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth), true);
    assert.deepEqual(reads.sort(), ['broken:alpha', 'broken:beta', 'healthy:alpha', 'healthy:beta']);
    assert.deepEqual(errors, []);
  } finally { await browser.close(); await server.close(); }
});
