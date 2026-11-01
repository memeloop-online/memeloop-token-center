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
    assert.equal(await page.locator('.group-list .active > span').textContent(), 'Kimi models');
    assert.equal(await page.locator('.group-list .active > small').textContent(), '0 members');
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

test('localized resource strategy searches real group accounts, orders by keyboard, preserves failed saves and rejects removed members', { timeout: 45_000 }, async context => {
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
    const page = await browser.newPage({ viewport: { width: 390, height: 844 } });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'zh-CN'));
    const writes: Record<string, any>[] = [];
    let failCatalog = true;
    await page.route('**/internal/v1/**', async route => {
      if (route.request().url().endsWith('/plugins/group-routing-strategies')) {
        if (failCatalog) { failCatalog = false; await route.fulfill({ status: 503, json: { error: { message: 'unavailable' } } }); return; }
        await route.fulfill({ json: [{ id: 'arbitrary-plugin', version: 'group-routing-v1', default: { ordered: [] }, schema: {
          type: 'object', title: 'Account preference', 'x-i18n': { 'zh-CN': { title: '优先使用指定账号' } }, properties: {
            ordered: { type: 'array', title: 'Preferred order', 'x-i18n': { 'zh-CN': { title: '账号使用顺序' } }, items: { type: 'string' }, uniqueItems: true,
              'x-resource-selector': { source: 'group-accounts', ordered: true } },
          },
        } }, { id: 'unknown-source', version: 'group-routing-v1', default: { ordered: [] }, schema: { type: 'object', title: 'Unavailable source', properties: { ordered: { type: 'array', title: 'Unavailable accounts', items: { type: 'string' }, 'x-resource-selector': { source: 'future-resources', ordered: true } } } } }] });
      } else if (route.request().method() === 'PUT') {
        writes.push(route.request().postDataJSON());
        await route.fulfill(writes.length === 1 ? { status: 503, json: { error: { message: 'unavailable' } } } : { json: {
          id: 'group', name: 'Group', member_ids: ['alpha', 'beta', 'gamma'], member_count: 3, created_at: 1, updated_at: 2, strategy_version: 3, routing_strategy: writes.at(-1)?.routing_strategy, routing_priority: writes.at(-1)?.routing_priority,
        } });
      } else await route.fulfill({ json: [] });
    });
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/group-strategy.html?resources`);
    await page.getByText('暂时无法加载策略。请重试，已填写的内容会保留。').waitFor();
    await page.getByRole('button', { name: '重试', exact: true }).click();
    const strategy = page.getByRole('combobox', { name: '组路由策略', exact: true });
    await strategy.selectOption({ label: '优先使用指定账号' });
    assert.equal(await strategy.locator('option').filter({ hasText: 'arbitrary-plugin' }).count(), 0);
    const picker = page.getByRole('combobox', { name: '账号使用顺序', exact: true });
    await picker.fill('outside');
    assert.equal(await page.getByRole('option', { name: /Outside group/ }).count(), 0);
    await picker.fill('Anthropic');
    await picker.press('ArrowDown'); await picker.press('Enter');
    await picker.fill('alpha');
    await picker.press('ArrowDown'); await picker.press('Enter');
    const rows = page.locator('.resource-selector-order li');
    assert.match(await rows.nth(0).innerText(), /Beta account/);
    await page.getByRole('button', { name: '提高 Alpha account 的优先级', exact: true }).focus();
    await page.keyboard.press('Enter');
    assert.match(await rows.nth(0).innerText(), /Alpha account/);
    await page.setViewportSize({ width: 1440, height: 1000 });
    await page.locator('.group-strategy-editor').evaluate(element => { element.style.width = '384px'; element.style.maxWidth = '100%'; });
    const geometry = await rows.nth(0).evaluate(element => ({
      rowHeight: element.getBoundingClientRect().height,
      nameWidth: element.querySelector('.resource-selector-name')!.getBoundingClientRect().width,
    }));
    assert.ok(geometry.nameWidth >= 200, 'account names remain readable inside a narrow desktop form column');
    assert.ok(geometry.rowHeight < 180, 'account labels must not wrap into a vertical column of single characters');
    await page.setViewportSize({ width: 390, height: 844 });
    const save = page.getByRole('button', { name: '保存组策略', exact: true });
    await save.click();
    await page.getByText('暂时无法保存策略。请重试，已填写的内容会保留。').waitFor();
    assert.equal(await rows.count(), 2);
    assert.deepEqual(writes[0].routing_strategy.config, { ordered: ['alpha', 'beta'] });
    await page.getByRole('button', { name: 'Remove Beta membership' }).click();
    await page.getByText('请移除列表中已不可用的账号，再保存策略。').waitFor();
    assert.equal(await save.isDisabled(), true);
    await page.getByRole('button', { name: '移除 账号已不在可用成员中', exact: true }).click();
    await save.click();
    await page.getByText('组策略已保存', { exact: true }).waitFor();
    assert.deepEqual(writes[1].routing_strategy.config, { ordered: ['alpha'] });
    await page.getByLabel('重叠组优先级', { exact: true }).fill('7');
    await strategy.selectOption('');
    await page.getByRole('button', { name: '恢复已保存的设置', exact: true }).click();
    assert.equal(await strategy.inputValue(), 'arbitrary-plugin');
    assert.equal(await page.getByLabel('重叠组优先级', { exact: true }).inputValue(), '0');
    assert.match(await rows.nth(0).innerText(), /Alpha account/);
    assert.equal(await rows.count(), 1);
    assert.equal(await page.getByRole('button', { name: '恢复已保存的设置', exact: true }).count(), 0);
    await strategy.selectOption('unknown-source');
    await page.getByText('当前无法选择此类资源。请刷新页面或选择其他策略。').waitFor();
    assert.equal(await save.isDisabled(), true, 'an unknown resource source cannot save even an empty selection');
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > window.innerWidth), false);
  } finally { await browser.close(); await server.close(); }
});

test('quota choice automatically configures a unique real window and maps one choice to both stored fields', { timeout: 45_000 }, async context => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    context.skip('Chromium not installed'); return;
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 1440, height: 1000 } });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'zh-CN'));
    const writes: Record<string, any>[] = [];
    let multiple = false;
    let failMetadata = false;
    await page.route('**/internal/v1/**', async route => {
      const url = route.request().url();
      if (url.endsWith('/plugins/group-routing-strategies')) await route.fulfill({ json: [{ id: 'quota-order', version: 'group-routing-v1', default: { service: '', allowance: '' }, schema: {
        type: 'object', title: '优先使用即将重置的账号', description: '优先使用额度即将重置的账号', properties: { service: { type: 'string' }, allowance: { type: 'string' } },
        'x-resource-choice': { source: 'group-quota-windows', bindings: { service: 'provider', allowance: 'window_id' }, autoSelectSingle: true },
      } }] });
      else if (url.endsWith('/upstreams/quota/windows')) {
        if (failMetadata) { await route.fulfill({ status: 503, json: { error: { message: 'unavailable' } } }); return; }
        assert.deepEqual(route.request().postDataJSON(), { account_ids: ['alpha', 'beta', 'gamma'], tenant_external_id: 'tenant' });
        await route.fulfill({ json: { results: ['alpha', 'beta', 'gamma'].map(upstream_account_id => ({ upstream_account_id, provider: 'openai-codex', windows: [{ id: 'code:primary_window', label: 'code:primary_window', period_seconds: 604800 }, ...(multiple ? [{ id: 'code:secondary_window', label: 'code:secondary_window', period_seconds: 18000 }] : [])] })) } });
      } else if (url.endsWith('/upstreams/quota/batch')) {
        throw new Error('Persisted windows must configure immediately without supplier discovery');
      } else if (route.request().method() === 'PUT') {
        writes.push(route.request().postDataJSON());
        await route.fulfill({ json: { id: 'group', name: 'Group', member_ids: ['alpha', 'beta', 'gamma'], member_count: 3, created_at: 1, updated_at: 2,
          strategy_version: 3, routing_strategy: writes.at(-1)?.routing_strategy, routing_priority: 0 } });
      } else await route.fulfill({ json: [] });
    });
    const url = `http://127.0.0.1:${address.port}/e2e/fixtures/group-strategy.html?resources`;
    await page.goto(url);
    await page.getByRole('combobox', { name: '组路由策略', exact: true }).selectOption('quota-order');
    const choice = page.getByRole('combobox', { name: '优先使用的额度', exact: true });
    await page.waitForFunction(() => document.querySelector<HTMLSelectElement>('.schema-resource-choice select')?.selectedOptions[0]?.textContent === 'Codex · 周额度');
    assert.equal(await page.locator('.group-strategy-editor input:not([type="hidden"]):not([type="number"])').count(), 0);
    await page.getByRole('button', { name: '查看分配方式' }).hover();
    await page.getByRole('tooltip').filter({ hasText: '优先使用额度即将重置的账号' }).waitFor();
    await page.getByRole('button', { name: '保存组策略', exact: true }).click();
    await page.getByText('组策略已保存', { exact: true }).waitFor();
    assert.deepEqual(writes[0].routing_strategy.config, { service: 'openai-codex', allowance: 'code:primary_window' });
    multiple = true; await page.goto(url);
    await page.getByRole('combobox', { name: '组路由策略', exact: true }).selectOption('quota-order');
    await choice.selectOption({ label: 'Codex · 5 小时额度' });
    await page.getByRole('button', { name: '保存组策略', exact: true }).click();
    await page.getByText('组策略已保存', { exact: true }).waitFor();
    assert.deepEqual(writes[1].routing_strategy.config, { service: 'openai-codex', allowance: 'code:secondary_window' });
    failMetadata = true;
    await page.getByRole('button', { name: 'Remove Beta membership' }).click();
    await page.getByText('额度窗口读取失败，请重试。').waitFor();
    assert.match(await choice.locator('option:checked').innerText(), /Codex/);
    await page.getByLabel('重叠组优先级', { exact: true }).fill('5');
    await page.getByRole('button', { name: '保存组策略', exact: true }).click();
    await page.getByText('组策略已保存', { exact: true }).waitFor();
    assert.deepEqual(writes[2].routing_strategy.config, { service: 'openai-codex', allowance: 'code:secondary_window' });
    assert.equal(writes[2].routing_priority, 5);
    await page.setViewportSize({ width: 390, height: 844 });
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false);
  } finally { await browser.close(); await server.close(); }
});
