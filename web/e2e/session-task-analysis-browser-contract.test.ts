import assert from 'node:assert/strict';
import { existsSync, mkdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';
import type {} from './fixtures/session-tasks-state.js';

test('session tasks: CAS rename protects concurrent names, relations manage, rollup scopes switch', { timeout: 60_000 }, async () => {
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
    const page = await browser.newPage({ timezoneId: 'UTC', viewport: { width: 1440, height: 1000 } });
    const evidence = fileURLToPath(new URL('../e2e-artifacts/usage-analysis/session-tasks/', import.meta.url));
    mkdirSync(evidence, { recursive: true });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/session-tasks.html`);

    // Manual rename uses compare-and-set with the observed revision.
    await page.getByRole('button', { name: 'Generate AI title', exact: true }).waitFor();
    await page.getByRole('button', { name: 'Rename', exact: true }).click();
    await page.getByRole('textbox', { name: 'Enter a session name' }).fill('Fix prod deploys');
    await page.getByRole('button', { name: 'Save name', exact: true }).click();
    await page.waitForFunction(() => window.namePuts.length === 1);
    const firstPut = await page.evaluate(() => window.namePuts[0]);
    assert.equal(firstPut.title, 'Fix prod deploys');
    assert.equal(firstPut.source, 'manual');
    assert.equal(firstPut.expected_revision, 0, 'first manual name starts from revision 0');
    await page.locator('[data-testid="title-echo"]').getByText('Fix prod deploys', { exact: true }).waitFor();

    // A concurrent namer wins: the conflict is explained and the latest name reloads.
    await page.evaluate(() => { window.forceNameConflict = true; });
    await page.getByRole('button', { name: 'Rename', exact: true }).click();
    await page.getByRole('textbox', { name: 'Enter a session name' }).fill('Other name');
    await page.getByRole('button', { name: 'Save name', exact: true }).click();
    await page.waitForFunction(() => window.namePuts.length === 2);
    const secondPut = await page.evaluate(() => window.namePuts[1]);
    assert.equal(secondPut.expected_revision, 1, 'second save carries the revision from the last read');
    await page.getByText('The name was changed elsewhere; the latest name is loaded.', { exact: true }).waitFor();
    assert.equal(await page.locator('[data-testid="title-echo"]').innerText(), 'Fix prod deploys', 'a lost rename never overwrites the displayed name');

    // Manual relations: add a related session, see its name, remove it again.
    await page.getByRole('button', { name: 'Session relations', exact: true }).click();
    await page.getByText('No related sessions yet.', { exact: true }).waitFor();
    await page.getByRole('button', { name: 'Add related session', exact: true }).click();
    await page.getByLabel('Copy session ID').fill('00000000-0000-4000-8000-000000000004');
    await page.getByLabel('Copy credential ID').fill('00000000-0000-4000-8000-000000000003');
    await page.getByRole('button', { name: 'Add related session', exact: true }).click();
    await page.waitForFunction(() => window.relationshipWrites.some((write) => write.method === 'PUT' && write.kind === 'related'));
    await page.getByText(/Related Beta · 00000003\/00000004/).waitFor();
    await page.getByRole('button', { name: 'Remove relation', exact: true }).click();
    await page.waitForFunction(() => window.relationshipWrites.some((write) => write.method === 'DELETE' && write.kind === 'related'));
    await page.getByText('No related sessions yet.', { exact: true }).waitFor();

    await page.getByRole('combobox', { name: 'Search by name or session ID, then select a session' }).click();
    await page.keyboard.type('Alpha');
    await page.getByRole('option', { name: /Parent task Alpha/ }).click();
    await page.waitForFunction(() => window.analyzeCalls.length === 1);
    assert.equal((await page.evaluate(() => window.analyzeCalls[0])).scope, 'session');
    assert.deepEqual((await page.evaluate(() => window.analyzeCalls[0])).identities, [
      { key_id: '00000000-0000-4000-8000-000000000001', session_id: '00000000-0000-4000-8000-000000000002' },
    ]);
    await page.getByText('First activity', { exact: true }).waitFor();
    await page.getByText('Active days', { exact: true }).waitFor();
    await page.getByText('$12.34', { exact: true }).first().waitFor();
    await page.getByText('10/24/2026', { exact: true }).waitFor();
    await page.getByText('10/25/2026', { exact: true }).waitFor();
    await page.getByText(/Day boundary zone: UTC/).waitFor();
    assert.equal((await page.evaluate(() => window.analyzeCalls[0])).timezone, 'UTC');
    await page.screenshot({ path: `${evidence}/desktop.png`, fullPage: true });
    await page.setViewportSize({ width: 390, height: 844 });
    await page.screenshot({ path: `${evidence}/mobile.png`, fullPage: true });

    await page.getByRole('button', { name: 'Include sub-sessions', exact: true }).click();
    await page.waitForFunction(() => window.analyzeCalls.length === 2);
    assert.equal((await page.evaluate(() => window.analyzeCalls[1])).scope, 'subtree');

    await page.getByRole('button', { name: 'Related sessions', exact: true }).click();
    await page.getByText('No related sessions yet. Add relations from the session details first.', { exact: true }).waitFor();
    assert.equal(await page.evaluate(() => window.analyzeCalls.length), 2, 'an empty relation set runs no rollup');

    await page.getByRole('button', { name: 'View separately', exact: true }).click();
    await page.waitForFunction(() => window.analyzeCalls.length === 5);
    await page.locator('.usage-task-separate').nth(1).getByText('First activity', { exact: true }).waitFor();
    assert.equal(await page.locator('.usage-task-separate').count(), 2, 'expanded subtree members each get a session rollup');
    const separateCalls = await page.evaluate(() => window.analyzeCalls.slice(3, 5));
    assert.deepEqual(separateCalls.map(call => call.scope), ['session', 'session']);
    assert.equal(new Set(separateCalls.map(call => call.identities[0].session_id)).size, 2);

    await page.evaluate(() => { window.relatedActive = true; });
    await page.getByRole('button', { name: 'Related sessions', exact: true }).click();
    await page.waitForFunction(() => window.analyzeCalls.length === 6);
    const union = await page.evaluate(() => window.analyzeCalls[5]);
    assert.equal(union.scope, 'related');
    assert.equal(union.identities.length, 2);
    assert.equal(union.include_subtrees, false);
    await page.getByRole('checkbox', { name: 'Include sub-sessions', exact: true }).check();
    await page.waitForFunction(() => window.analyzeCalls.length === 7);
    assert.equal((await page.evaluate(() => window.analyzeCalls[6])).include_subtrees, true);
    await page.getByRole('checkbox', { name: /Related Beta/ }).uncheck();
    await page.locator('.usage-task-metrics').waitFor({ state: 'hidden' });
    assert.equal(await page.locator('.usage-task-metrics').count(), 0, 'deselection removes the previous union total');

    await page.evaluate(() => { window.holdAnalysis = true; });
    await page.getByRole('button', { name: 'Include sub-sessions', exact: true }).click();
    await page.waitForFunction(() => window.analyzeCalls.length === 8);
    await page.getByRole('button', { name: 'This session', exact: true }).click();
    await page.waitForFunction(() => window.analyzeCalls.length === 9);
    await page.locator('.usage-task-metrics b').first().getByText('3', { exact: true }).waitFor();
    await page.evaluate(async () => {
      window.releaseAnalysis();
      await new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)));
    });
    assert.equal(await page.locator('.usage-task-metrics b').first().innerText(), '3', 'late subtree result cannot replace this-session totals');

    await page.evaluate(() => { window.holdAnalysis = true; });
    await page.getByRole('button', { name: 'Include sub-sessions', exact: true }).click();
    await page.waitForFunction(() => window.analyzeCalls.length === 10);
    await page.getByRole('button', { name: 'Cancel', exact: true }).click();
    await page.getByText('Loading cancelled. Retry or select another scope.', { exact: false }).waitFor();
    await page.evaluate(async () => {
      window.releaseAnalysis();
      await new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)));
    });
    assert.equal(await page.locator('.usage-task-metrics').count(), 0, 'cancelled result stays hidden even if transport delivers it');

    const zonedPage = await browser.newPage({ timezoneId: 'America/New_York' });
    await zonedPage.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await zonedPage.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/session-tasks.html`);
    await zonedPage.getByRole('combobox').fill('Alpha');
    await zonedPage.getByRole('option', { name: /Parent task Alpha/ }).click();
    await zonedPage.getByText('11/1/2026', { exact: true }).waitFor();
    await zonedPage.getByText('11/2/2026', { exact: true }).waitFor();
    const zonedCall = await zonedPage.evaluate(() => window.analyzeCalls[0]);
    assert.equal(zonedCall.timezone, 'America/New_York');
    assert.equal('timezone_offset_minutes' in zonedCall, false, 'IANA calls never send a competing fixed offset');
    await zonedPage.screenshot({ path: `${evidence}/new-york-dst.png`, fullPage: true });
  } finally {
    await browser.close();
    await server.close();
  }
});
