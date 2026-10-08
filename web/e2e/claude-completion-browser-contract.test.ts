import assert from 'node:assert/strict';
import { editProviderAccount } from './support/provider-account-navigation.js';
import { fileURLToPath } from 'node:url';
import test, { type TestContext } from 'node:test';
import { chromium } from 'playwright';
import { authorizationJourneyCopy } from '../src/operator/authorizationJourneyCopy.js';
import { claudeCompletionLimits } from '../src/operator/authorizationCode.js';
import { createIsolatedFixtureServer } from './support/isolated-vite-server.js';

declare global {
  interface Window {
    claudePendingFixture: {
      initializedAt: number;
      calls: { at: number; body: unknown; aborted: boolean }[];
      release: (index: number, status: number, body: unknown) => void;
    };
  }
}

async function fixture(context: TestContext, { locale = 'en', reauthorize = false, expiresIn = 600_000, scopeControls = false } = {}) {
  const server = await createIsolatedFixtureServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  context.after(() => server.close());
  await server.listen();
  const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const origin = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  context.after(() => browser.close());
  const page = await browser.newPage();
  const pageErrors: string[] = [];
  page.on('pageerror', error => pageErrors.push(error.message));
  context.after(() => assert.deepEqual(pageErrors, []));
  const time = new Date('2026-10-06T12:00:00Z');
  await page.clock.install({ time });
  await page.clock.setFixedTime(time);
  await page.clock.pauseAt(time);
  await page.clock.setSystemTime(time);
  assert.equal(await page.evaluate(() => Date.now()), time.getTime(), 'the clock is paused at the original epoch before navigation');
  await page.addInitScript(value => localStorage.setItem('mtc-locale', value), locale);
  await page.addInitScript(`(() => {
    const previous = window.fetch;
    const releases = [];
    const state = window.claudePendingFixture = {
      initializedAt: Date.now(),
      calls: [],
      release: (index, status, body) => releases[index](new Response(JSON.stringify(body), { status }))
    };
    window.fetch = (input, init) => {
      const url = new URL(typeof input === 'string' ? input : input instanceof URL ? input.href : input.url, location.origin);
      if (url.pathname !== '/internal/v1/oauth/claude/complete') return previous(input, init);
      const call = { at: Date.now(), body: JSON.parse(init.body), aborted: false };
      state.calls.push(call);
      init.signal.addEventListener('abort', () => { call.aborted = true; }, { once: true });
      return new Promise(resolve => releases.push(resolve));
    };
  })()`);
  const account = { id: 'claude-pending-fixture', tenant_id: 'fixture-tenant-id', tenant_external_id: 'fixture-a', name: 'Fixture Claude account',
    driver: 'anthropic-claude', auth_kind: 'oauth', connection_method: 'oauth', status: 'active', credential_generation: 3,
    credential_expires_at: null, can_refresh: true, can_rotate: false, can_reauthorize: true, route_count: 0, config: {}, created_at: 1, updated_at: 3 };
  let listedAccounts = reauthorize ? [{ ...account, credential_generation: 2 }] : [];
  let reads = 0;
  let statisticsReads = 0;
  let starts = 0;
  await page.route('**/*', async route => {
    const request = route.request(); const url = new URL(request.url());
    assert.equal(url.origin, origin, 'only the synthetic local fixture may receive traffic');
    if (!url.pathname.startsWith('/internal/')) return route.continue();
    if (request.method() === 'GET') {
      if (url.pathname.includes('monitoring') || url.pathname.includes('availability')) { statisticsReads++; return route.fulfill({ status: 503, json: { error: { message: 'Fixture statistics unavailable' } } }); }
      if (url.pathname === '/internal/v1/provider-types') return route.fulfill({ json: [{ id: 'anthropic-claude', display_name: 'Fixture Claude', source: 'builtin', protocols: ['anthropic'], modalities: ['text'], config_schema: { type: 'object' }, credential_schema: { type: 'object', properties: { type: { const: 'oauth' } } }, oauth_adapter: { flow_kind: 'claude_manual_pkce' } }] });
      if (url.pathname === '/internal/v1/upstreams') { reads++; return route.fulfill({ json: listedAccounts }); }
      return route.fulfill({ json: [] });
    }
    assert.equal(request.method(), 'POST');
    assert.equal(url.pathname, '/internal/v1/oauth/claude/start');
    assert.equal(request.postDataJSON().upstream_account_id, reauthorize ? account.id : undefined);
    starts++;
    return route.fulfill({ json: { session_token: 'synthetic-pending-session', login_url: `${origin}/mock-provider`, expires_at: time.getTime() + expiresIn } });
  });
  await page.goto(`${origin}/e2e/fixtures/authorization-code.html?${scopeControls ? 'scope-controls' : 'full-page'}`);
  assert.deepEqual(await page.evaluate(() => ({ initializedAt: window.claudePendingFixture.initializedAt, now: Date.now() })), { initializedAt: time.getTime(), now: time.getTime() }, 'application initialization must observe the original paused epoch');
  const chinese = locale.startsWith('zh');
  const add = page.getByRole('button', { name: chinese ? '新增上游' : 'Add upstream', exact: true });
  if (reauthorize) {
    await editProviderAccount(page, account.id);
    await page.getByRole('button', { name: chinese ? '重新授权' : 'Authorize again', exact: true }).click();
  } else {
    await add.click();
    await page.getByRole('button', { name: chinese ? '账户授权' : 'Account authorization', exact: true }).click();
  }
  const workspace = page.locator('.create-journey:has(.authorization-form)');
  const code = workspace.locator('.manual-authorization input');
  const complete = workspace.getByRole('button', { name: chinese ? '完成授权' : 'Complete authorization', exact: true });
  const start = workspace.getByRole('button', { name: chinese ? '开始登录' : 'Start login', exact: true });
  const close = workspace.getByRole('button', { name: chinese ? '关闭' : 'Close', exact: true });
  const copy = authorizationJourneyCopy(locale);
  await start.click();
  await code.fill('synthetic-code#synthetic-state');
  const calls = () => page.evaluate(() => window.claudePendingFixture.calls);
  const release = (index: number, status: number, body: unknown) => {
    if ((status === 200 || status === 201) && body && typeof body === 'object' && 'credential_generation' in body) {
      assert.deepEqual(body, account, 'only the complete synthetic account may be saved');
      listedAccounts = [{ ...account }];
    }
    return page.evaluate(value => window.claudePendingFixture.release(value.index, value.status, value.body), { index, status, body });
  };
  const pending = async (index: number, seconds: number) => {
    await release(index, 202, { status: 'pending', retry_after_seconds: seconds });
    await workspace.getByText(copy.completePending, { exact: true }).waitFor();
  };
  const checking = () => workspace.getByText(copy.completeChecking, { exact: true }).waitFor();
  const retained = async () => {
    assert.equal(await code.inputValue(), 'synthetic-code#synthetic-state');
    assert.equal(await workspace.locator('.authorization-form').getByRole('button', { name: chinese ? '开始登录' : 'Start login', exact: true, includeHidden: true }).isDisabled(), true);
    assert.equal(await workspace.locator('.notice.success').count(), 0);
    assert.doesNotMatch(await page.evaluate(() => JSON.stringify([localStorage, sessionStorage])), /synthetic-code|synthetic-state|synthetic-pending-session/);
    assert.equal(starts, 1);
  };
  return { page, account, workspace, code, complete, start, close, add, copy, calls, release, pending, checking, retained, reads: () => reads, statisticsReads: () => statisticsReads, starts: () => starts };
}

test('Claude fixture starts at the original paused epoch and Date advances exactly with explicit ticks', { timeout: 60_000 }, async context => {
  const journey = await fixture(context);
  const epoch = Date.parse('2026-10-06T12:00:00Z');
  assert.equal(await journey.page.evaluate(() => Date.now()), epoch);
  await journey.page.clock.runFor(999);
  assert.equal(await journey.page.evaluate(() => Date.now()), epoch + 999);
  await journey.page.clock.runFor(1);
  assert.equal(await journey.page.evaluate(() => Date.now()), epoch + 1000);
  assert.equal((await journey.calls()).length, 0);
  await journey.retained();
});

for (const locale of ['zh-CN', 'en']) test(`Claude pending stays in the authorization workspace until a real account arrives (${locale})`, { timeout: 60_000 }, async context => {
  const journey = await fixture(context, { locale, reauthorize: true });
  const reads = journey.reads();
  await journey.page.clock.runFor(10_000);
  assert.equal((await journey.calls()).length, 0, 'a session and code alone cannot start automatic completion');
  await journey.complete.evaluate(button => { (button as HTMLButtonElement).click(); (button as HTMLButtonElement).click(); });
  await journey.checking();
  assert.equal((await journey.calls()).length, 1);
  await journey.pending(0, 5);
  await journey.retained();
  assert.equal(journey.reads(), reads);
  assert.equal(await journey.complete.isDisabled(), true);
  assert.equal(await journey.code.isDisabled(), true);
  await journey.complete.evaluate(button => { (button as HTMLButtonElement).click(); (button as HTMLButtonElement).click(); });
  await journey.page.clock.runFor(4999);
  assert.equal((await journey.calls()).length, 1, 'no request before the server interval');
  await journey.page.clock.runFor(1);
  await journey.checking();
  await journey.complete.evaluate(button => { (button as HTMLButtonElement).click(); (button as HTMLButtonElement).click(); });
  assert.equal((await journey.calls()).length, 2);
  await journey.pending(1, 7);
  await journey.retained();
  assert.equal(journey.reads(), reads, 'repeated pending cannot refresh the parent');
  await journey.page.clock.runFor(6999);
  assert.equal((await journey.calls()).length, 2);
  await journey.page.clock.runFor(1);
  await journey.checking();
  const calls = await journey.calls();
  assert.equal(calls.length, 3);
  assert.ok(calls[1].at - calls[0].at >= 5000);
  assert.ok(calls[2].at - calls[1].at >= 7000);
  for (const call of calls) assert.deepEqual(call.body, { session_token: 'synthetic-pending-session', authorization_code: 'synthetic-code#synthetic-state' });
  await journey.retained();
  assert.equal(journey.reads(), reads);
  await journey.release(2, 200, journey.account);
  await journey.workspace.waitFor({ state: 'detached' });
  const accountWorkspace = journey.page.locator(`[id="provider-details-${journey.account.id}"]`);
  await accountWorkspace.getByRole('heading', { name: journey.account.name, exact: true }).waitFor();
  assert.equal(await journey.page.locator('.provider-detail-workspace').count(), 1);
  assert.equal(await journey.page.locator('.provider-edit-workspace').count(), 0);
  const successNotice = accountWorkspace.getByText(journey.copy.saved, { exact: true });
  await successNotice.waitFor();
  assert.equal(await successNotice.count(), 1);
  assert.equal(journey.reads(), reads + 1);
  await journey.page.clock.runFor(60_000);
  assert.equal((await journey.calls()).length, 3);
  assert.equal(journey.starts(), 1);
});

for (const locale of ['zh-CN', 'en']) test(`Claude pending stops on an actionable safe error and retains the draft (${locale})`, { timeout: 60_000 }, async context => {
  const journey = await fixture(context, { locale });
  const reads = journey.reads();
  await journey.complete.click();
  await journey.pending(0, 5);
  await journey.page.clock.runFor(5000);
  await journey.checking();
  await journey.release(1, 403, { error: { code: 'forbidden', message: 'synthetic-error-message-canary', authorization_code: 'synthetic-error-code-canary', state: 'synthetic-error-state-canary', session_token: 'synthetic-error-token-canary' } });
  await journey.workspace.getByRole('alert').getByText(journey.copy.completeForbidden, { exact: true }).waitFor();
  assert.doesNotMatch(await journey.page.locator('body').innerHTML(), /synthetic-error-(?:message|code|state|token)-canary/);
  await journey.retained();
  assert.equal(await journey.complete.isEnabled(), true);
  assert.equal(await journey.code.isEnabled(), true);
  await journey.page.clock.runFor(60_000);
  assert.equal((await journey.calls()).length, 2);
  assert.equal(journey.reads(), reads);
});

for (const reopenAfter of [4000, 12_000]) test(`closing a confirmed pending create journey preserves its one continuation on reopen after ${reopenAfter}ms`, { timeout: 60_000 }, async context => {
  const journey = await fixture(context);
  const reads = journey.reads();
  await journey.complete.click();
  await journey.pending(0, 10);
  await journey.close.click();
  await journey.code.waitFor({ state: 'hidden' });
  await journey.page.clock.runFor(reopenAfter);
  assert.equal((await journey.calls()).length, 1);
  await journey.retained();
  await journey.add.click();
  if (reopenAfter < 10_000) {
    await journey.workspace.getByText(journey.copy.completePending, { exact: true }).waitFor();
    await journey.page.clock.runFor(10_000 - reopenAfter - 1);
    assert.equal((await journey.calls()).length, 1, 'reopening cannot shorten the confirmed server interval');
  }
  await journey.page.clock.runFor(1);
  await journey.checking();
  const calls = await journey.calls();
  assert.equal(calls.length, 2);
  assert.ok(calls[1].at - calls[0].at >= 10_000);
  assert.deepEqual(calls[1].body, calls[0].body);
  await journey.retained();
  assert.equal(journey.reads(), reads);
  assert.equal(journey.starts(), 1, 'reopening continues the original authorization');
  await journey.release(1, 200, journey.account);
  await journey.code.waitFor({ state: 'detached' });
  await journey.page.locator('.create-journey[data-open="false"]').waitFor();
  await journey.workspace.getByRole('region', { includeHidden: true }).waitFor({ state: 'hidden' });
  const savedAccount = journey.page.locator(`[data-upstream-id="${journey.account.id}"]`);
  const accountWorkspace = journey.page.locator(`[id="provider-details-${journey.account.id}"]`);
  await accountWorkspace.getByRole('heading', { name: journey.account.name, exact: true }).waitFor();
  assert.equal(await accountWorkspace.getAttribute('aria-label'), `${journey.account.name} · Manage account`);
  assert.equal(await journey.page.locator('.provider-detail-workspace').count(), 1);
  assert.equal(await savedAccount.isVisible(), false, 'the created account workspace replaces its directory');
  assert.equal(await savedAccount.count(), 1);
  const success = journey.page.getByText(`Saved upstream connection ${journey.account.name}.`, { exact: true });
  await success.waitFor();
  assert.equal(await success.count(), 1);
  assert.equal(await journey.page.locator('.create-journey').getAttribute('data-open'), 'false');
  assert.equal(journey.reads(), reads + 1);
  await journey.page.clock.runFor(60_000);
  assert.equal((await journey.calls()).length, 2);
  assert.equal(journey.starts(), 1);
  await journey.add.click();
  await journey.start.waitFor();
  assert.equal(await journey.code.count(), 0, 'successful completion cannot restore the authorization code on reopen');
  assert.doesNotMatch(await journey.page.locator('body').innerHTML(), /synthetic-code|synthetic-state|synthetic-pending-session/);
  assert.doesNotMatch(await journey.page.evaluate(() => JSON.stringify([localStorage, sessionStorage])), /synthetic-code|synthetic-state|synthetic-pending-session/);
  assert.equal((await journey.calls()).length, 2);
  assert.equal(journey.starts(), 1);
  assert.equal(journey.reads(), reads + 1);
});

for (const locale of ['zh-CN', 'en']) for (const status of [200, 202, 403]) for (const deliverWhileClosed of [true, false]) test(`closing a dispatched Claude continuation cannot reuse an older pending result (${locale}, late ${status}, ${deliverWhileClosed ? 'closed' : 'reopened'})`, { timeout: 60_000 }, async context => {
  const journey = await fixture(context, { locale });
  await journey.complete.click();
  await journey.pending(0, 5);
  await journey.page.clock.runFor(5000);
  await journey.checking();
  const calls = await journey.calls();
  assert.equal(calls.length, 2);
  assert.deepEqual(calls[1].body, calls[0].body);
  assert.equal(await journey.workspace.getByText(journey.copy.completePending, { exact: true }).count(), 0);
  const reads = journey.reads();
  const statisticsReads = journey.statisticsReads();
  await journey.close.click();
  await journey.code.waitFor({ state: 'hidden' });
  assert.equal((await journey.calls())[1].aborted, true, 'only the browser abort is observed; the synthetic server can still respond');
  const response = status === 200 ? journey.account : status === 202 ? { status: 'pending', retry_after_seconds: 1 }
    : { error: { code: 'forbidden', message: 'synthetic-error-message-canary', authorization_code: 'synthetic-error-code-canary', state: 'synthetic-error-state-canary', session_token: 'synthetic-error-token-canary' } };
  if (deliverWhileClosed) {
    await journey.release(1, status, response);
    await journey.page.clock.runFor(100);
    await journey.code.waitFor({ state: 'hidden' });
    await journey.retained();
    assert.equal((await journey.calls()).length, 2);
    assert.equal(journey.reads(), reads);
    assert.equal(journey.statisticsReads(), statisticsReads);
  }
  await journey.add.click();
  const unknownNotice = journey.workspace.getByRole('alert').getByText(journey.copy.completeClosedUnknown, { exact: true });
  await unknownNotice.waitFor();
  assert.equal(await unknownNotice.count(), 1);
  await journey.retained();
  await journey.page.clock.runFor(10_000);
  assert.equal((await journey.calls()).length, 2, 'reopening cannot authorize a third request from the first pending response');
  if (!deliverWhileClosed) await journey.release(1, status, response);
  await journey.page.clock.runFor(60_000);
  await unknownNotice.waitFor();
  assert.equal(await unknownNotice.count(), 1);
  assert.equal(await journey.workspace.getByText(journey.copy.completePending, { exact: true }).count(), 0);
  assert.equal(await journey.workspace.getByText(journey.copy.completeChecking, { exact: true }).count(), 0);
  assert.doesNotMatch(await journey.page.locator('body').innerHTML(), /synthetic-error-(?:message|code|state|token)-canary/);
  await journey.retained();
  assert.equal((await journey.calls()).length, 2, 'late success, pending and error responses cannot restore continuation permission');
  assert.equal(journey.reads(), reads);
  assert.equal(journey.statisticsReads(), statisticsReads);
  assert.equal(journey.starts(), 1);
});

test('closing pending reauthorization preserves the original account and fences a late completion', { timeout: 60_000 }, async context => {
  const journey = await fixture(context, { reauthorize: true });
  const reads = journey.reads();
  await journey.complete.click();
  await journey.pending(0, 1);
  await journey.page.clock.runFor(1000);
  await journey.checking();
  await journey.close.click();
  await journey.workspace.waitFor({ state: 'detached' });
  const accountWorkspace = journey.page.locator(`[id="provider-details-${journey.account.id}"]`);
  const heading = accountWorkspace.getByRole('heading', { name: journey.account.name, exact: true });
  await heading.waitFor();
  assert.equal(await journey.page.locator('.provider-edit-workspace').count(), 0);
  assert.equal((await journey.calls())[1].aborted, true);
  await journey.release(1, 200, journey.account);
  await journey.page.clock.runFor(60_000);
  assert.equal(journey.reads(), reads);
  assert.equal(await heading.count(), 1);
  assert.equal(await journey.page.locator('.provider-detail-workspace').count(), 1);
  assert.equal(await journey.page.getByText(journey.copy.saved, { exact: true }).count(), 0);
  assert.equal((await journey.calls()).length, 2);
  assert.equal(journey.starts(), 1);
});

for (const control of ['Switch tenant', 'Switch credential']) test(`pending Claude completion cannot cross scope: ${control}`, { timeout: 60_000 }, async context => {
  const journey = await fixture(context, { scopeControls: true });
  await journey.complete.click();
  await journey.pending(0, 1);
  await journey.page.clock.runFor(1000);
  await journey.checking();
  await journey.page.getByRole('button', { name: control, exact: true }).click();
  await journey.code.waitFor({ state: 'detached' });
  await journey.add.waitFor();
  assert.equal((await journey.calls())[1].aborted, true);
  await journey.add.click();
  await journey.page.getByRole('button', { name: 'Account authorization', exact: true }).click();
  await journey.start.waitFor();
  await journey.page.clock.runFor(100);
  const reads = journey.reads();
  await journey.release(1, 200, journey.account);
  await journey.page.clock.runFor(60_000);
  assert.equal(journey.reads(), reads);
  assert.equal(await journey.start.isEnabled(), true);
  assert.equal(await journey.code.count(), 0);
  assert.equal(await journey.workspace.locator('.notice.success').count(), 0);
  assert.equal((await journey.calls()).length, 2);
  assert.equal(journey.starts(), 1);
});

for (const expiry of [true, false]) test(`pending Claude checks stop at the ${expiry ? 'session expiry' : 'local deadline'} without shortening the server delay`, { timeout: 60_000 }, async context => {
  const journey = await fixture(context, { expiresIn: expiry ? 10_000 : 600_000 });
  const reads = journey.reads();
  await journey.complete.click();
  await journey.pending(0, 300);
  await journey.page.clock.runFor((expiry ? 10_000 : claudeCompletionLimits.durationMillis) - 1);
  assert.equal((await journey.calls()).length, 1);
  await journey.page.clock.runFor(1);
  await journey.workspace.getByRole('alert').getByText(expiry ? journey.copy.completeExpired : journey.copy.completeLimited, { exact: true }).waitFor();
  await journey.retained();
  assert.equal(await journey.complete.isDisabled(), true);
  await journey.page.clock.runFor(600_000);
  assert.equal((await journey.calls()).length, 1);
  assert.equal(journey.reads(), reads);
});

test('repeated Claude pending responses exhaust a finite attempt budget', { timeout: 60_000 }, async context => {
  const journey = await fixture(context);
  const reads = journey.reads();
  await journey.complete.click();
  for (let index = 0; index < claudeCompletionLimits.attempts; index++) {
    await journey.checking();
    assert.equal((await journey.calls()).length, index + 1);
    await journey.pending(index, 1);
    await journey.page.clock.runFor(1000);
  }
  await journey.workspace.getByRole('alert').getByText(journey.copy.completeLimited, { exact: true }).waitFor();
  await journey.retained();
  assert.equal(await journey.complete.isDisabled(), true);
  await journey.page.clock.runFor(600_000);
  assert.equal((await journey.calls()).length, claudeCompletionLimits.attempts);
  assert.equal(journey.reads(), reads);
});

test('a hung Claude completion is bounded even when fetch ignores abort, and its late result is ignored', { timeout: 60_000 }, async context => {
  const journey = await fixture(context);
  const reads = journey.reads();
  await journey.complete.click();
  await journey.pending(0, 1);
  await journey.page.clock.runFor(1000);
  await journey.checking();
  await journey.page.clock.runFor(claudeCompletionLimits.requestMillis);
  await journey.workspace.getByRole('alert').getByText(journey.copy.completeUncertain, { exact: true }).waitFor();
  assert.equal((await journey.calls())[1].aborted, true);
  await journey.retained();
  await journey.release(1, 200, journey.account);
  await journey.page.clock.runFor(60_000);
  await journey.retained();
  assert.equal((await journey.calls()).length, 2);
  assert.equal(journey.reads(), reads);
});

test('an incomplete successful Claude response cannot clear the draft or refresh the parent', { timeout: 60_000 }, async context => {
  const journey = await fixture(context);
  const reads = journey.reads();
  await journey.complete.click();
  await journey.release(0, 200, { id: journey.account.id });
  await journey.workspace.getByRole('alert').getByText(journey.copy.completeUncertain, { exact: true }).waitFor();
  await journey.retained();
  assert.equal(await journey.complete.isEnabled(), true);
  await journey.page.clock.runFor(60_000);
  assert.equal((await journey.calls()).length, 1);
  assert.equal(journey.reads(), reads);
});
