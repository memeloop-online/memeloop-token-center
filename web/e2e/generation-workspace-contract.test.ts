import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { mkdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium, type Page } from 'playwright';
import { createIsolatedFixtureServer as createServer } from './support/isolated-vite-server.js';

const baseJob = {
  created_at: 1_700_000_000_000,
  updated_at: 1_700_000_000_000,
  completed_at: null,
  driver: 'http-json',
  billing_unit: 'job',
  upstream_job_id: null,
  estimated_units: 1,
  billed_units: 1,
  cost: '0.29',
  error_code: null,
  result: null,
  assets: [],
  tenant_external_id: 'alpha',
  key_id: '019f0000-0000-7000-8000-000000000098',
  key_alias: 'Fixture key',
  currency: 'USD',
};
const queuedJob = { ...baseJob, job_id: '019f0000-0000-7000-8000-000000000099', model: 'fixture-video-queued', status: 'queued' };
const doneJob = { ...baseJob, job_id: '019f0000-0000-7000-8000-000000000097', model: 'fixture-image-done', status: 'succeeded', completed_at: 1_700_000_100_000 };

async function settle(page: Page) {
  await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
}

test('generation workspace keeps job lifecycle distinct from request traffic and stacks on mobile', { timeout: 60_000 }, async context => {
  const executablePath = process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH ?? chromium.executablePath();
  if (!existsSync(executablePath)) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required');
    return context.skip('Chromium runtime is required');
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer!.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ executablePath, headless: true });
  const artifacts = fileURLToPath(new URL('../e2e-artifacts/ui-system/generation-workspace/', import.meta.url));
  await mkdir(artifacts, { recursive: true });
  try {
    const page = await browser.newPage({ viewport: { width: 390, height: 900 } });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.route('**/internal/v1/generations?**', route => route.fulfill({ json: [queuedJob, doneJob] }));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/generation-workspace.html`);
    const panel = page.locator('.operator-generations');
    await panel.getByText('fixture-video-queued').waitFor();
    assert.equal(await panel.getByRole('button', { name: 'More actions', exact: true }).count(), 0,
      'generation jobs do not hide request review behind a generic menu');
    assert.equal(await panel.getByRole('button', { name: 'Image requests requiring review', exact: true }).count(), 0,
      'request reconciliation belongs to Requests, not generation jobs');

    // Truthful status copy: queued reads Pending, succeeded reads Completed.
    const queuedRow = panel.locator('tbody tr', { hasText: 'fixture-video-queued' });
    const doneRow = panel.locator('tbody tr', { hasText: 'fixture-image-done' });
    await queuedRow.getByText('Pending', { exact: true }).waitFor();
    await doneRow.getByText('Completed', { exact: true }).waitFor();
    assert.equal(await doneRow.getByRole('button', { name: 'Cancel', exact: true }).count(), 0, 'finished jobs render no cancel action');
    assert.equal(await queuedRow.getByRole('button', { name: 'Cancel', exact: true }).isEnabled(), true);

    // Visual artifacts through the real application shell: theme x width.
    for (const theme of ['light', 'dark']) for (const width of [390, 1440]) {
      await page.evaluate(value => { document.documentElement.dataset.theme = value; }, theme);
      await page.setViewportSize({ width, height: 900 });
      await settle(page);
      await page.screenshot({ path: `${artifacts}generation-workspace-${theme}-${width}.png` });
    }
    await page.evaluate(() => { document.documentElement.dataset.theme = 'light'; });
    await page.setViewportSize({ width: 390, height: 900 });
    await settle(page);

    // Mobile stacking: model, status and actions visible without horizontal scrolling.
    assert.equal(await page.locator('.generation-table').evaluate(el => getComputedStyle(el).display), 'block');
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth), true, 'no document-level horizontal overflow');
    for (const selector of ['.generation-model-cell', '.generation-status-cell', '.generation-actions-cell']) {
      const box = await doneRow.locator(selector).boundingBox();
      assert.ok(box && box.x >= 0 && box.x + box.width <= 390, `${selector} fully inside the 390px viewport`);
    }
    // A scope change keeps the job workspace authoritative for its tenant.
    await page.getByLabel('Fixture tenant').selectOption('beta');
    await panel.getByText('fixture-video-queued').waitFor();
  } finally { await browser.close(); await server.close(); }
});
