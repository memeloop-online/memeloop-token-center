import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer as createServer } from './support/isolated-vite-server.js';

test('group strategy schema validation, CAS refresh preservation, native reset and credential isolation', { timeout: 45_000 }, async context => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    context.skip('Chromium not installed'); return;
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    page.on('pageerror', error => context.diagnostic(error.message));
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    const writes: Record<string, any>[] = [];
    let catalogReads = 0;
    await page.route('**/internal/v1/**', async route => {
      const url = route.request().url();
      if (url.endsWith('/plugins/group-routing-strategies')) {
        catalogReads++;
        await route.fulfill({ json: [{ id: 'weighted', version: 'group-routing-v1', default: { factor: 3 }, schema: { type: 'object', required: ['factor'], properties: { factor: { type: 'integer', title: 'Weight factor', minimum: 1 } } } }] });
      } else if (route.request().method() === 'PUT') {
        writes.push(route.request().postDataJSON());
        await route.fulfill(writes.length === 1 ? { status: 409, json: { error: { message: 'conflict' } } } : { json: { id: 'group', name: 'Group', member_ids: [], member_count: 0, created_at: 1, updated_at: 4, strategy_version: 5 } });
      } else await route.fulfill({ json: [{ id: 'group', name: 'Group', member_ids: [], member_count: 0, created_at: 1, updated_at: 3, strategy_version: 4 }] });
    });
    const base = `http://127.0.0.1:${address.port}/e2e/fixtures/group-strategy.html`;
    await page.goto(base);
    const picker = page.getByRole('combobox', { name: 'Group routing strategy' });
    assert.equal(await page.locator('.group-strategy-editor form').count(), 0, 'native ordering has no empty configuration form');
    await picker.selectOption('weighted');
    const factor = page.getByLabel('Weight factor', { exact: false });
    assert.equal(await factor.inputValue(), '3');
    assert.equal(await page.locator('.group-strategy-editor textarea').count(), 0);
    await factor.fill('0');
    await page.getByRole('button', { name: 'Save group strategy' }).click();
    assert.equal(writes.length, 0, 'schema invalid configuration cannot be sent');
    await factor.fill('7');
    await page.getByLabel('Overlapping group priority').fill('10');
    await page.getByRole('button', { name: 'External strategy update', exact: true }).click();
    await page.waitForFunction(() => document.querySelector<HTMLInputElement>('.group-rename input')?.value === 'Group externally updated');
    assert.equal(await factor.inputValue(), '7', 'external strategy version cannot replace the local draft');
    await page.getByRole('button', { name: 'Save group strategy' }).click();
    await page.getByText(/Its version was refreshed/).waitFor();
    assert.equal(await factor.inputValue(), '7');
    assert.equal(await page.getByLabel('Overlapping group priority').inputValue(), '10');
    assert.equal(writes[0].expected_strategy_version, 2);
    assert.equal(writes[0].expected_updated_at, 1, 'external strategy update must preserve the old CAS and produce the explicit conflict, not silently authorize overwriting it');
    await page.getByRole('button', { name: 'Save group strategy' }).click();
    await page.getByText('Group strategy saved', { exact: true }).waitFor();
    assert.equal(writes[1].expected_strategy_version, 4);
    assert.equal(writes[1].expected_updated_at, 3);
    assert.deepEqual(writes[1].routing_strategy, { plugin_id: 'weighted', config: { factor: 7 } });
    await picker.selectOption('');
    assert.equal(await page.locator('.group-strategy-editor form').count(), 0);
    await page.getByRole('button', { name: 'Save group strategy' }).click();
    await page.waitForFunction(() => document.querySelector('.group-strategy-editor [role="status"]')?.textContent === 'Group strategy saved');
    assert.equal(writes[2].routing_strategy, null);
    const readsBefore = catalogReads;
    await page.goto(`${base}?kind=credential`);
    await page.getByRole('heading', { name: 'Credential groups' }).waitFor();
    assert.equal(await page.locator('.group-strategy-editor').count(), 0);
    assert.equal(catalogReads, readsBefore, 'credential groups do not request strategy catalog');

    await page.unroute('**/internal/v1/**');
    let releaseList!: () => void;
    let listStarted!: () => void;
    const listRequested = new Promise<void>(resolve => { listStarted = resolve; });
    const listGate = new Promise<void>(resolve => { releaseList = resolve; });
    const oldGroup = { id: 'group', name: 'CSiL', member_ids: ['sol', 'terra', 'luna'], member_count: 3, created_at: 1, updated_at: 1 };
    let newGroup = { id: 'new-group', name: 'Kimi models', member_ids: [] as string[], member_count: 0, created_at: 2, updated_at: 2 };
    const staleNewGroup = { ...newGroup };
    const memberWrites: { path: string; body: Record<string, any> }[] = [];
    await page.route('**/internal/v1/**', async route => {
      const request = route.request(), path = new URL(request.url()).pathname;
      if (path.endsWith('/plugins/group-routing-strategies')) return route.fulfill({ json: [] });
      if (request.method() === 'POST') return route.fulfill({ status: 201, json: newGroup });
      if (request.method() === 'PUT') {
        const body = request.postDataJSON(); memberWrites.push({ path, body });
        if (path.endsWith('/routing-strategy')) newGroup = { ...newGroup, updated_at: 5 };
        else if (path.endsWith('/members')) newGroup = { ...newGroup, member_ids: body.member_ids, member_count: body.member_ids.length, updated_at: 4 };
        else newGroup = { ...newGroup, name: body.name, updated_at: 3 };
        return route.fulfill({ json: newGroup });
      }
      listStarted(); await listGate;
      await route.fulfill({ json: [oldGroup, staleNewGroup] });
    });
    await page.goto(`${base}?lifecycle=1`);
    await page.locator('.selection-chip-label').filter({ hasText: 'sol' }).waitFor();
    await page.locator('.group-create input').fill('Kimi models');
    await page.getByRole('button', { name: 'Create group', exact: true }).click();
    await listRequested;
    await page.locator('.group-list .active').filter({ hasText: 'Kimi models' }).waitFor();
    assert.equal(await page.locator('.group-rename input').inputValue(), 'Kimi models');
    assert.equal(await page.locator('.selection-chip-label').count(), 0, 'the new group must never borrow the first group members while list reload is pending');
    releaseList();
    await page.waitForFunction(() => !document.querySelector<HTMLButtonElement>('.group-editor-actions button')?.disabled);
    assert.equal(await page.locator('.group-list .active').innerText(), 'Kimi models\n0 members');
    const members = page.getByRole('combobox', { name: 'Provider members' });
    await members.fill('kimi'); await members.press('Enter'); await members.press('Escape');
    await page.locator('.group-rename input').fill('Kimi renamed');
    await page.locator('.group-rename').getByRole('button', { name: 'Save', exact: true }).click();
    await page.locator('.group-list .active').filter({ hasText: 'Kimi renamed' }).waitFor();
    await page.waitForFunction(() => !document.querySelector<HTMLButtonElement>('.group-editor-actions button')?.disabled);
    assert.equal(await page.locator('.group-rename input').inputValue(), 'Kimi renamed', 'stale CAS2 list cannot revert the successful CAS3 rename');
    assert.deepEqual(await page.locator('.selection-chip-label').allTextContents(), ['kimi'], 'rename and stale list preserve unsaved member draft');
    await page.getByRole('button', { name: 'Save members', exact: true }).click();
    await page.getByText('Group members saved', { exact: true }).waitFor();
    assert.deepEqual(memberWrites, [
      { path: '/internal/v1/provider-groups/new-group', body: { tenant_external_id: 'tenant', name: 'Kimi renamed', expected_updated_at: 2 } },
      { path: '/internal/v1/provider-groups/new-group/members', body: { tenant_external_id: 'tenant', member_ids: ['kimi'], expected_updated_at: 3 } },
    ], 'rename and member saves target only the created group with its latest CAS');
    await page.waitForFunction(() => !document.querySelector<HTMLButtonElement>('.group-editor-actions button')?.disabled);
    assert.deepEqual(await page.locator('.selection-chip-label').allTextContents(), ['kimi']);
    await page.getByRole('button', { name: 'Save group strategy', exact: true }).click();
    await page.getByText('Group strategy saved', { exact: true }).waitFor();
    assert.equal(memberWrites[2].path, '/internal/v1/provider-groups/new-group/routing-strategy');
    assert.equal(memberWrites[2].body.expected_updated_at, 4, 'strategy CAS adopts the authoritative member-save revision despite stale list reads');
    const ordering = await page.evaluate(() => document.querySelector('.group-editor-actions')!.compareDocumentPosition(document.querySelector('.group-strategy-editor')!));
    assert.ok(ordering & 4, 'member editing precedes advanced strategy configuration');

    await page.unroute('**/internal/v1/**');
    let releaseCreate!: () => void;
    let createStarted!: () => void;
    const createRequested = new Promise<void>(resolve => { createStarted = resolve; });
    const createGate = new Promise<void>(resolve => { releaseCreate = resolve; });
    await page.route('**/internal/v1/**', async route => {
      if (route.request().method() === 'POST') { createStarted(); await createGate; return route.fulfill({ status: 201, json: newGroup }); }
      await route.fulfill({ json: [] });
    });
    await page.goto(`${base}?lifecycle=1`);
    await page.locator('.group-create input').fill('Late group');
    const lateResponse = page.waitForResponse(response => response.request().method() === 'POST');
    await page.getByRole('button', { name: 'Create group', exact: true }).click();
    await createRequested;
    await page.getByRole('button', { name: 'Switch tenant', exact: true }).click();
    await page.locator('.group-list .active').filter({ hasText: 'Other tenant' }).waitFor();
    releaseCreate(); await (await lateResponse).finished();
    await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
    assert.equal(await page.locator('.group-rename input').inputValue(), 'Other tenant');
    assert.equal(await page.locator('.group-list [role="listitem"]').count(), 1, 'a late creation response cannot insert a group into another tenant');
    assert.equal(await page.locator('.selection-chip-label').count(), 0);
  } finally { await browser.close(); await server.close(); }
});
