import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';
declare global { interface Window { requestLifecycleFixture: { finish: () => void; hold: () => void; release: () => void; switchScope: () => void; filter: () => void; failQuery: () => void; held: boolean; queryHeld: boolean; detailCalls: number; scopeCommits: { scope: string; drawers: number }[] }; } }

test('request error details distinguish active delivery from recorded terminal failure', { timeout: 45_000 }, async (context) => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required');
    context.skip('Chromium is not installed'); return;
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const origin = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    await page.route('**/*', route => {
      const url = new URL(route.request().url());
      return url.origin === origin && !url.pathname.startsWith('/internal/') ? route.continue() : route.abort();
    });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`${origin}/e2e/fixtures/request-lifecycle.html`);
    const open = page.locator('tbody tr').filter({ has: page.getByText('model-a', { exact: true }) }).locator('.table-action');
    await open.waitFor();
    for (const mode of ['delivery', 'terminal', 'unknown'] as const) {
      await page.evaluate(mode => {
        const previous = window.fetch;
        window.fetch = async (input, init) => {
          const url = new URL(typeof input === 'string' ? input : input instanceof URL ? input.href : input.url, location.origin);
          if (url.pathname !== '/internal/v1/requests/request-a') return previous(input, init);
          const detail = await (await previous(input, init)).json();
          return new Response(JSON.stringify({ ...detail,
            status_code: mode === 'delivery' ? null : 502,
            completed_at: mode === 'delivery' ? null : 3000,
            error_code: mode === 'delivery' ? 'delivery_started' : mode === 'terminal' ? 'upstream_stream' : null,
            terminal_cause_code: mode === 'terminal' ? 'upstream_transport_connection_reset' : null,
          }));
        };
      }, mode);
      await open.click();
      const drawer = page.getByRole('dialog', { name: 'model-a' });
      await drawer.locator(`[data-outcome="${mode === 'delivery' ? 'delivering' : mode === 'terminal' ? 'interrupted' : 'failed'}"]`).waitFor();
      const text = await drawer.innerText();
      if (mode === 'delivery') {
        assert.match(text, /Awaiting settlement/);
        assert.doesNotMatch(text, /The request failed|delivery_started/);
        assert.equal(await drawer.getByText('Error', { exact: true }).count(), 0, 'a progress marker must not create an error detail row');
      } else {
        const cause = mode === 'terminal' ? 'The upstream connection was reset' : 'Unknown (no specific cause recorded)';
        assert.equal(text.split(cause).length - 1, 1);
        assert.doesNotMatch(text, /upstream_transport_connection_reset|upstream_stream/);
        await drawer.locator('.request-outcome').focus();
        const tooltip = page.getByRole('tooltip').filter({ hasText: cause });
        await tooltip.waitFor();
        assert.equal(await tooltip.innerText(), `Recorded cause: ${cause}`);
      }
      await drawer.getByRole('button', { name: 'Close', exact: true }).click();
    }
  } finally { await browser.close(); await server.close(); }
});

test('request detail follows terminal events and fences late responses after selection and scope changes', { timeout: 45_000 }, async (t) => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required');
    t.skip('Chromium is not installed'); return;
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage({ hasTouch: true });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/request-lifecycle.html`);
    const imageReviewEntry = page.locator('.request-page-surface').getByRole('button', { name: 'Image requests requiring review', exact: true });
    await imageReviewEntry.click();
    await page.getByRole('button', { name: 'Open image request review', exact: true }).waitFor();
    assert.equal(await imageReviewEntry.getAttribute('aria-expanded'), 'true');
    await imageReviewEntry.click();
    assert.equal(await page.getByRole('button', { name: 'Open image request review', exact: true }).count(), 0,
      'request reconciliation is available from Requests and remains closed until explicitly opened');
    const first = page.locator('tbody tr').filter({ has: page.getByText('model-a', { exact: true }) });
    await page.getByRole('tooltip').filter({ hasText: 'Background scope help' }).waitFor();
    await first.locator('.table-action').click();
    await page.locator('.drawer [data-outcome="running"]').waitFor();
    await page.evaluate(() => {
      const node = document.createElement('div');
      node.id = 'already-inert-background'; node.inert = true; node.setAttribute('aria-hidden', 'false');
      document.body.append(node);
    });
    await page.waitForFunction(() => document.getElementById('already-inert-background')?.getAttribute('aria-hidden') === 'true');
    assert.equal(await page.locator('.request-compaction').count(), 0, 'missing compaction evidence creates no badge');
    assert.equal(await page.getByRole('tooltip').filter({ hasText: 'Background scope help' }).count(), 0, 'other-scope floating content remains excluded while the drawer is open');
    assert.equal(await page.locator('#background-help').evaluate((element) => !!element.closest('[inert]')), true, 'background controls remain inert');
    await page.locator('.drawer .request-outcome').tap();
    const ownedTooltip = page.getByRole('tooltip').filter({ hasText: 'No terminal outcome is recorded yet' });
    await ownedTooltip.waitFor();
    assert.equal(await ownedTooltip.evaluate((element) => !!element.closest('.drawer-owned-portals') && !element.closest('[inert], [aria-hidden="true"]')), true, 'only the drawer-owned portal remains in the accessible modal subtree');
    await page.locator('.drawer .close').focus();
    await ownedTooltip.waitFor({ state: 'hidden' });
    const status = page.locator('.drawer .request-outcome');
    await status.press('Shift+Tab');
    await page.keyboard.press('Tab');
    assert.equal(await status.evaluate(element => element === document.activeElement), true, 'Tab focuses the request outcome control');
    await ownedTooltip.waitFor();
    assert.equal(await ownedTooltip.evaluate((element) => !!element.closest('.drawer-owned-portals') && !element.closest('[inert], [aria-hidden="true"]')), true, 'the keyboard-triggered tooltip remains in the accessible modal subtree');
    assert.match(await page.locator('.drawer .request-diagnostics').first().innerText(), /Awaiting settlement/);
    const modelDetails = page.locator('.drawer .request-detail-wide .request-metadata-trigger').first();
    await modelDetails.tap();
    const metadata = page.locator('.request-metadata-surface');
    await metadata.waitFor({ state: 'visible' });
    assert.equal(await metadata.evaluate(element => !!element.closest('.drawer-owned-portals') && !element.closest('[inert], [aria-hidden="true"]')), true, 'technical metadata belongs to the current accessible drawer');
    await metadata.getByRole('button').first().focus();
    await page.keyboard.press('Escape');
    await metadata.waitFor({ state: 'detached' });
    assert.equal(await page.locator('.drawer').count(), 1, 'Escape closes the supplemental metadata, not the request drawer');
    const drawerClose = page.locator('.drawer .close');
    const technical = page.getByRole('button', { name: 'Technical details', exact: true });
    await drawerClose.focus();
    await page.keyboard.press('Shift+Tab');
    assert.equal(await technical.evaluate(element => element === document.activeElement), true, 'backward Tab wraps to the last drawer action');
    await page.keyboard.press('Tab');
    assert.equal(await drawerClose.evaluate(element => element === document.activeElement), true, 'forward Tab wraps to the first drawer action');
    await page.evaluate(() => window.requestLifecycleFixture.finish());
    await page.locator('.drawer [data-outcome="completed"]').waitFor();
    assert.equal(await drawerClose.evaluate(element => element === document.activeElement), true, 'terminal re-render and updated close callback do not reset drawer focus');
    await page.locator('.drawer .request-compaction').waitFor();
    assert.equal(await first.locator('.request-compaction').count(), 1, 'SSE and detail use the same explicit compaction evidence');
    await page.locator('.drawer .request-compaction').tap();
    await page.getByRole('tooltip').filter({ hasText: 'The client explicitly marked this request as context compaction.' }).waitFor();
    const liveHelp = page.locator('[role="tooltip"]').filter({ hasText: 'Live-added background help' });
    await page.waitForFunction(() => [...document.querySelectorAll('[role="tooltip"]')].some(element => element.textContent === 'Live-added background help' && !!element.closest('[inert][aria-hidden="true"]')));
    assert.equal(await page.getByRole('tooltip').filter({ hasText: 'Live-added background help' }).count(), 0, 'a newly mounted background portal is excluded while the drawer is open');
    await technical.focus();
    await page.keyboard.press('Enter');
    assert.equal(await technical.getAttribute('aria-expanded'), 'true');
    assert.ok(await page.locator('.request-technical-details pre').count() >= 2, 'explicit technical action retains complete request and response data');
    assert.equal(await page.locator('details.request-technical-details').count(), 0, 'technical data needs no native disclosure triangle');
    assert.equal(await page.evaluate(() => window.requestLifecycleFixture.detailCalls), 2);
    await page.evaluate(() => { window.requestLifecycleFixture.hold(); window.requestLifecycleFixture.finish(); });
    await page.waitForFunction(() => window.requestLifecycleFixture.held);
    await page.getByRole('button', { name: 'Close', exact: true }).click();
    await page.getByRole('tooltip').filter({ hasText: 'Background scope help' }).waitFor();
    await page.getByRole('tooltip').filter({ hasText: 'Live-added background help' }).waitFor();
    assert.equal(await liveHelp.evaluate(element => !!element.closest('[inert], [aria-hidden="true"]')), false, 'new background portal regains its original accessibility on close');
    assert.deepEqual(await page.locator('#already-inert-background').evaluate(element => ({ inert: element instanceof HTMLElement && element.inert, hidden: element.getAttribute('aria-hidden') })), { inert: true, hidden: 'false' }, 'cleanup restores original attributes, not generic accessibility defaults');
    await page.locator('#already-inert-background').evaluate(element => element.remove());
    assert.equal(await page.locator('.drawer-owned-portals').count(), 0, 'closing removes only the owned portal subtree');
    assert.equal(await page.locator('#background-help').evaluate((element) => !!element.closest('[inert], [aria-hidden="true"]')), false, 'closing restores background accessibility');
    await page.locator('tbody tr').filter({ has: page.getByText('model-b', { exact: true }) }).locator('.table-action').click();
    await page.getByRole('dialog', { name: 'model-b' }).waitFor();
    await page.evaluate(() => window.requestLifecycleFixture.release());
    await page.evaluate(() => new Promise<void>((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
    assert.equal(await page.getByRole('dialog', { name: 'model-b' }).count(), 1, 'late request-a response cannot replace request-b');
    await page.getByRole('button', { name: 'Close', exact: true }).click();
    await first.locator('.table-action').click();
    await page.getByRole('dialog', { name: 'model-a' }).waitFor();
    await page.evaluate(() => { window.requestLifecycleFixture.hold(); window.requestLifecycleFixture.finish(); });
    await page.waitForFunction(() => window.requestLifecycleFixture.held);
    await page.evaluate(() => window.requestLifecycleFixture.switchScope());
    await page.locator('[data-request-fixture-scope="tenant-b"] tbody tr').first().waitFor();
    assert.deepEqual(await page.evaluate(() => window.requestLifecycleFixture.scopeCommits.filter((entry) => entry.scope === 'tenant-b')), [{ scope: 'tenant-b', drawers: 0 }], 'the first new-scope layout commit must not expose old detail before effects clear it');
    await page.evaluate(() => window.requestLifecycleFixture.release());
    await page.evaluate(() => new Promise<void>((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
    assert.equal(await page.getByRole('dialog').count(), 0, 'late detail cannot reopen across tenant scope');
    await first.locator('.table-action').click();
    await page.getByRole('dialog', { name: 'model-a' }).waitFor();
    assert.equal(await page.locator('.load-more button').count(), 1, 'the old result must have a pagination cursor before replacement');
    await page.evaluate(() => window.requestLifecycleFixture.filter());
    await page.waitForFunction(() => window.requestLifecycleFixture.queryHeld);
    assert.equal(await page.locator('tbody tr').count(), 0, 'old rows are hidden while replacement is pending');
    assert.equal(await page.locator('.load-more button').count(), 0, 'old cursor is not actionable while replacement is pending');
    assert.equal(await page.getByRole('dialog').count(), 0, 'filter replacement closes the old request detail');
    await page.evaluate(() => window.requestLifecycleFixture.failQuery());
    await page.getByRole('alert').waitFor();
    assert.equal(await page.locator('tbody tr').count(), 0, 'a failed replacement must not restore stale rows');
    assert.equal(await page.locator('.load-more button').count(), 0, 'a failed replacement must not restore its cursor');
    assert.equal(await page.getByRole('dialog').count(), 0, 'a failed replacement must not reopen old detail');
    for (const theme of ['dark', 'light']) {
      await page.evaluate((value) => { document.documentElement.dataset.theme = value; }, theme);
      const surface = await page.locator('.request-page-surface').evaluate((element) => ({ border: getComputedStyle(element).borderTopWidth, shadow: getComputedStyle(element).boxShadow }));
      assert.deepEqual(surface, { border: '0px', shadow: 'none' });
    }
    // A directory failure must not become a request-list failure or hide rows.
    // Exercise the actual fetch/catch/render path, not the implementation's helper name.
    const localized = await browser.newPage();
    await localized.addInitScript(() => localStorage.setItem('mtc-locale', 'zh-CN'));
    await localized.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/request-lifecycle.html?directory-error`);
    await localized.getByRole('alert').waitFor();
    await localized.locator('tbody tr').first().waitFor();
    const alert = await localized.getByRole('alert').innerText();
    assert.match(alert, /上游目录: HTTP 503/);
    assert.match(alert, /关联请求 ID: 01900000-0000-7000-8000-000000000001/);
    assert.doesNotMatch(alert, /请求列表:|secret-directory-body-canary|request ID:/);
    assert.equal(await localized.locator('tbody tr').count(), 2);
    await localized.locator('tbody tr').first().locator('.table-action').click();
    await localized.getByRole('dialog').waitFor();
    assert.equal(await localized.getByRole('alert').count(), 0, 'the background directory alert must remain isolated by the modal');
    assert.equal(await localized.getByRole('alert', { includeHidden: true }).textContent(), alert, 'opening a successful detail must not relabel or clear the background directory error');
    await localized.getByRole('dialog').getByRole('button', { name: '关闭', exact: true }).click();
    await localized.getByRole('alert').waitFor();
    assert.equal(await localized.getByRole('alert').innerText(), alert, 'closing the modal restores access to the same directory error');
    await localized.close();
  } finally { await browser.close(); await server.close(); }
});


test('supplier cause stays shared through list, keyboard tooltip, detail and terminal refresh in both locales', { timeout: 45_000 }, async (context) => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required');
    context.skip('Chromium is not installed'); return;
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const origin = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  try {
    for (const [locale, reason, prefix, close] of [
      ['zh-CN', '当前上游账号没有可用套餐，需在提供商处开通或更换账号。', '已记录原因', '关闭'],
      ['en', 'The upstream account has no active plan. Activate a plan with the provider or use another account.', 'Recorded cause', 'Close'],
    ] as const) {
      const page = await browser.newPage();
      await page.route('**/*', route => new URL(route.request().url()).origin === origin ? route.continue() : route.abort());
      await page.addInitScript(value => localStorage.setItem('mtc-locale', value), locale);
      await page.goto(`${origin}/e2e/fixtures/request-lifecycle.html?supplier-error`);
      const row = page.locator('tbody tr').filter({ has: page.getByText('model-a', { exact: true }) });
      await row.locator('[data-outcome="failed"]').waitFor();
      assert.equal((await row.innerText()).split(reason).length - 1, 1);
      await row.locator('.request-outcome').focus();
      const tooltip = page.getByRole('tooltip').filter({ hasText: reason });
      await tooltip.waitFor();
      assert.equal(await tooltip.innerText(), `${prefix}: ${reason}`);
      assert.equal(await page.evaluate(() => window.requestLifecycleFixture.detailCalls), 0, 'list reason and keyboard tooltip need no detail fetch');
      await row.locator('.table-action').click();
      const drawer = page.getByRole('dialog', { name: 'model-a' });
      await drawer.getByText(`${prefix}: ${reason}`, { exact: true }).waitFor();
      assert.equal((await drawer.innerText()).split(reason).length - 1, 1);
      assert.doesNotMatch(await drawer.innerText(), /http_402|no_active_plan|请求失败|The request failed/);
      await drawer.locator('.request-outcome').focus();
      await tooltip.waitFor();
      assert.equal(await tooltip.innerText(), `${prefix}: ${reason}`);
      assert.equal(await page.evaluate(() => window.requestLifecycleFixture.detailCalls), 1);
      await page.evaluate(() => window.requestLifecycleFixture.finish());
      await page.waitForFunction(() => window.requestLifecycleFixture.detailCalls === 2);
      await drawer.getByText(`${prefix}: ${reason}`, { exact: true }).waitFor();
      assert.equal((await row.innerText()).split(reason).length - 1, 1, 'enriched event preserves the list cause after detail refresh');
      await drawer.locator('.close').focus();
      await drawer.locator('.request-outcome').press('Shift+Tab');
      await page.keyboard.press('Tab');
      assert.equal(await drawer.locator('.request-outcome').evaluate(element => element === document.activeElement), true);
      await tooltip.waitFor();
      assert.equal(await tooltip.innerText(), `${prefix}: ${reason}`);
      assert.equal(await page.evaluate(() => window.requestLifecycleFixture.detailCalls), 2, 'status interaction adds no fetch beyond existing terminal refresh');
      await drawer.getByRole('button', { name: close, exact: true }).click();
      await page.close();
    }
  } finally { await browser.close(); await server.close(); }
});
