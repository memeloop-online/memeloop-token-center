import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { exactMicros } from '../src/operator/quarantineAmount.js';
import { createIsolatedFixtureServer as createServer } from './support/isolated-vite-server.js';

test('manual currency conversion is exact and rejects invalid or unsafe inputs', () => {
  assert.equal(exactMicros('0.100001'), 100001);
  assert.equal(exactMicros('9007199254.740991'), Number.MAX_SAFE_INTEGER);
  for (const invalid of ['9007199254.740992', '-1', '1e3', '0.0000001', 'Infinity', '', ' 1', '1.']) assert.equal(exactMicros(invalid), undefined);
});

test('quarantine review requires facts, preserves conflict draft and retry identity, isolates tenants', { timeout: 45000 }, async context => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    context.skip('Chromium not installed'); return;
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    const writes: { body: Record<string, unknown>; key: string }[] = [];
    let reads = 0;
    let failDetail = false;
    const item = { request_id: 'request-alpha', tenant_external_id: 'alpha', model: 'image', currency: 'USD', reserved_micros: 1000000, submission_started_at: 1700000000000, submission_uncertain_at: 1700000001000, revision: 'a'.repeat(64), status: 'awaiting_confirmation', resolution: null };
    await page.route('**/internal/v1/image-generation-quarantine**', async route => {
      const req = route.request(); const url = new URL(req.url());
      if (req.method() === 'POST') {
        writes.push({ body: req.postDataJSON(), key: req.headers()['idempotency-key'] });
        if (writes.length === 1) { await route.abort('failed'); return; }
        if (writes.length === 2) { item.revision = 'b'.repeat(64); await route.fulfill({ status: 409, json: { error: { message: 'conflict' } } }); return; }
        if (writes.length === 5) { failDetail = true; await route.fulfill({ status: 409, json: { error: { message: 'conflict' } } }); return; }
        await route.fulfill({ json: { ...req.postDataJSON(), request_id: item.request_id, resolution_id: 'receipt', resolved_by_service_id: 'service-alpha', resulting_status: 'failed', created_at: 1700000002000 } }); return;
      }
      reads++;
      if (failDetail && url.pathname.endsWith('/request-alpha')) { failDetail = false; await route.fulfill({ status: 503, json: { error: { message: 'unavailable' } } }); return; }
      await route.fulfill({ json: url.pathname.endsWith('/request-alpha') ? item : url.searchParams.get('tenant_external_id') === 'alpha' ? [item] : [] });
    });
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/image-quarantine.html`);
    const selectDetails = async () => {
      await page.getByRole('button', { name: 'Details', exact: true }).click();
      // Busy and resolved forms are disabled. Do not edit the previous draft
      // while the selected detail request is still replacing its state.
      await page.locator('[aria-label="Manual review details"] fieldset:not([disabled])').waitFor();
    };
    await page.getByText('Select an explicit tenant first;', { exact: false }).waitFor(); assert.equal(reads, 0);
    await page.getByLabel('Fixture tenant').selectOption('alpha');
    const openReview = page.getByRole('button', { name: 'Open manual review (tenant service credential required)', exact: true });
    await openReview.waitFor();
    assert.equal(reads, 0, 'selecting a tenant does not implicitly request privileged quarantine data');
    await openReview.click();
    await selectDetails();
    await page.getByLabel('Verified resolution').selectOption('settle_confirmed');
    await page.getByLabel('Confirmed amount').fill('-1');
    await page.getByText('Enter a nonnegative decimal', { exact: false }).waitFor();
    await page.getByLabel('Confirmed amount').fill('0.100001');
    await page.getByLabel('Evidence digest', { exact: true }).fill('c'.repeat(64));
    const submit = page.getByRole('button', { name: 'Confirm manual resolution', exact: true });
    assert.equal(await submit.isEnabled(), false);
    const verified = page.getByRole('checkbox');
    const send = async () => { await verified.check(); await submit.click(); await page.getByRole('dialog').getByRole('button', { name: 'Confirm and continue', exact: true }).click(); };
    await send(); await page.getByRole('alert').waitFor();
    await send(); await page.getByText('The record changed.', { exact: false }).waitFor();
    assert.equal(writes[0].key, writes[1].key); assert.deepEqual(writes[0].body, writes[1].body);
    assert.equal(writes[0].body.confirmed_cost_micros, 100001);
    assert.equal(await page.getByLabel('Confirmed amount').inputValue(), '0.100001');
    assert.equal(await verified.isChecked(), false);
    await send(); await page.getByRole('status').waitFor();
    await page.getByRole('heading', { name: 'Manual resolution audit receipt' }).waitFor();
    assert.equal(await submit.isEnabled(), false, 'resolved receipts cannot be submitted again');
    await page.getByText('Acting service credential ID: service-alpha').waitFor();
    assert.equal(writes[2].body.expected_revision, 'b'.repeat(64)); assert.notEqual(writes[1].key, writes[2].key);
    await page.getByRole('button', { name: 'Refresh', exact: true }).click();
    await selectDetails();
    // Details loads asynchronously. The previous resolved form also has a
    // zero amount, but still has the old action; wait for the new form state.
    await page.locator('input[inputmode="decimal"][readonly]').waitFor();
    assert.equal(await page.getByLabel('Confirmed amount').inputValue(), '0');
    assert.equal(await page.getByLabel('Confirmed amount').evaluate(node => (node as HTMLInputElement).readOnly), true);
    await page.getByLabel('Evidence digest', { exact: true }).fill('d'.repeat(64));
    await send(); await page.getByRole('status').waitFor();
    assert.equal(writes[3].body.action, 'not_delivered'); assert.equal(writes[3].body.confirmed_cost_micros, 0);
    await page.getByRole('button', { name: 'Refresh', exact: true }).click();
    await selectDetails();
    await page.getByLabel('Evidence digest', { exact: true }).fill('e'.repeat(64));
    await send(); await page.getByText('The record conflicted and refresh failed.', { exact: false }).waitFor();
    await verified.check(); assert.equal(await submit.isEnabled(), false);
    assert.equal(await page.getByLabel('Evidence digest', { exact: true }).inputValue(), 'e'.repeat(64));
    await selectDetails();
    await page.getByLabel('Evidence digest', { exact: true }).fill('f'.repeat(64));
    await verified.check(); await submit.click(); await page.getByRole('dialog').waitFor();
    // Programmatic scope change models the parent authority changing while a modal is open.
    await page.getByLabel('Fixture tenant').evaluate((node) => {
      const select = node as HTMLSelectElement; select.value = 'beta'; select.dispatchEvent(new Event('change', { bubbles: true }));
    });
    await openReview.waitFor();
    const beforeOpen = reads;
    await openReview.click();
    await page.getByText('No image requests await review', { exact: false }).waitFor();
    assert.equal(reads, beforeOpen + 1, 'changed scope requires a fresh explicit open before reading');
    assert.equal(await page.getByText('request-alpha', { exact: false }).count(), 0);
    assert.equal(await page.getByRole('dialog').count(), 0);
    assert.equal(await page.getByLabel('Confirmed amount').count(), 0, 'old tenant form is unmounted, not merely empty');
    assert.equal(await page.getByLabel('Evidence digest', { exact: true }).count(), 0, 'old tenant evidence input is absent');
    assert.equal(writes.length, 5);
  } finally { await browser.close(); await server.close(); }
});
