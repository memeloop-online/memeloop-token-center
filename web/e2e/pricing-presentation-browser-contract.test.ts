import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

test('pricing comparison, deferred usage, provenance and editor drafts remain truthful and accessible', { timeout: 60_000 }, async context => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    context.skip('Chromium required'); return;
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 390, height: 844 } });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/pricing-presentation.html`);
    const unlimited = page.getByRole('region', { name: 'Unlimited credential' });
    await unlimited.waitFor();
    assert.doesNotMatch(await unlimited.innerText(), /4,294|9,007|1,234/);
    assert.match(await unlimited.innerText(), /RPM Unlimited/);
    const prepaid = await page.getByRole('region', { name: 'Prepaid credential' }).innerText();
    assert.match(prepaid, /4,294,967,295/);
    assert.match(prepaid, /9,007,199,254,740,991/);
    assert.match(prepaid, /Not set/);
    assert.doesNotMatch(prepaid, /Unlimited/);
    assert.doesNotMatch(await page.locator('body').innerText(), /cpamp:|copied:/);
    assert.equal(await page.getByText('models.dev', { exact: true }).isVisible(), true);
    const details = page.locator('.price-provenance').first();
    await details.focus();
    await page.getByRole('tooltip').waitFor();
    assert.match(await page.getByRole('tooltip').innerText(), /cpamp:import-run/);
    assert.equal(await page.locator('details').count(), 0);
    for (const width of [390, 1440]) {
      await page.setViewportSize({ width, height: 900 });
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
    }
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/pricing-presentation.html?workspace`);
    const table = page.locator('.token-pricing-table');
    await table.getByText('active-model', { exact: true }).waitFor();
    assert.equal(await table.locator('tbody tr').count(), 2, 'prices render before deferred usage');
    assert.match(await table.innerText(), /Loading/);
    assert.doesNotMatch(await table.innerText(), /No requests|default/);
    assert.equal(await page.locator('.manual-pricing [role="region"]').isVisible(), false);
    await page.getByRole('button', { name: 'Complete usage', exact: true }).click();
    await table.getByText('missing-model', { exact: true }).waitFor();
    assert.match(await table.locator('tr').filter({ hasText: 'unused-model' }).innerText(), /No requests/);
    await page.getByLabel('Search models or sources').fill('unused');
    assert.equal(await table.locator('tbody tr').count(), 1);
    await page.getByRole('button', { name: 'Clear filters', exact: true }).click();
    await page.getByLabel('Show', { exact: true }).selectOption('missing');
    assert.equal(await table.locator('tbody tr').count(), 1);
    assert.match(await table.innerText(), /missing-model/);
    await page.getByRole('button', { name: 'Clear filters', exact: true }).click();
    const manual = page.locator('.manual-pricing');
    await manual.getByRole('button', { name: 'Set model prices', exact: true }).click();
    const model = manual.getByRole('combobox', { name: 'Model', exact: true });
    await model.fill('fixture-edit');
    const input = manual.getByLabel('Input / million tokens*', { exact: true });
    const output = manual.getByLabel('Output / million tokens*', { exact: true });
    await input.fill('2'); await output.fill('8');
    assert.equal(await manual.getByRole('option', { name: 'Standard', exact: true }).count(), 1);
    await manual.getByRole('button', { name: 'Close', exact: true }).click();
    await manual.getByRole('button', { name: 'Set model prices', exact: true }).click();
    assert.equal(await model.inputValue(), 'fixture-edit');
    assert.equal(await input.inputValue(), '2', 'closing preserves draft prices');
    await manual.getByRole('button', { name: 'Save manual price', exact: true }).click();
    await manual.getByRole('alert').filter({ hasText: 'Fixture price rejected' }).waitFor();
    assert.equal(await input.inputValue(), '2', 'server error preserves draft');
    assert.equal(await output.inputValue(), '8');
    for (const width of [390, 1440]) {
      await page.setViewportSize({ width, height: 900 });
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
    }
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/pricing-presentation.html?workspace`);
    await page.locator('.token-pricing-table').getByText('active-model', { exact: true }).waitFor();
    await page.getByRole('button', { name: 'Fail usage', exact: true }).click();
    await page.getByRole('alert').filter({ hasText: 'Request statistics are unavailable' }).waitFor();
    assert.equal(await page.locator('.token-pricing-table tbody tr').count(), 2);
    assert.doesNotMatch(await page.locator('.token-pricing-table').innerText(), /No requests/);
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/pricing-presentation.html?table-states`);
    await page.getByRole('button', { name: 'Reload prices', exact: true }).click();
    assert.doesNotMatch(await page.locator('.token-pricing-table').innerText(), /Missing/);
    await page.getByLabel('Show', { exact: true }).selectOption('missing');
    assert.equal(await page.locator('.token-pricing-table tbody tr').count(), 0, 'unresolved price pages do not prove missing prices');
    await page.getByRole('button', { name: 'Clear filters', exact: true }).click();
    await page.getByLabel('Show', { exact: true }).selectOption('used');
    assert.equal(await page.locator('.token-pricing-table tbody tr').count(), 1);
    await page.getByRole('button', { name: 'Reload usage', exact: true }).click();
    assert.equal(await page.locator('.token-pricing-table tbody tr').count(), 0, 'active used filter never silently expands during reload');
    await page.getByRole('button', { name: 'Reject usage', exact: true }).click();
    await page.getByRole('status').filter({ hasText: 'used-model filter cannot be applied' }).waitFor();
    assert.equal(await page.getByLabel('Show', { exact: true }).inputValue(), 'used');
    assert.equal(await page.locator('.token-pricing-table tbody tr').count(), 0);
    assert.equal(await page.getByRole('option', { name: 'Missing prices', exact: true }).isDisabled(), true, 'failed usage cannot establish complete missing-model coverage');
  } finally { await browser.close(); await server.close(); }
});
