import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer } from './support/isolated-vite-server.js';

const copy = {
  'zh-CN': {
    start: '开始登录', openAuthorization: '打开授权页', check: '检查授权结果', countdown: /秒后可检查/,
    method: '账户授权', reauthorize: '重新授权', backToSetup: '返回登录设置', reload: '重新读取账号列表',
    savedListUnavailable: '账号已保存，但列表暂时无法读取。请重试读取，无需重新登录。',
    expired: '本次登录已过期。请返回登录设置后重新开始。',
    hint: '请在 Kimi 页面完成确认，然后检查授权结果。', validUntil: /有效期至/,
    security: '才在 Kimi 页面确认', openaiSecurity: /才在 OpenAI 页面继续/,
  },
  en: {
    start: 'Start login', openAuthorization: 'Open authorization', check: 'Check authorization', countdown: /Check in \d+s/,
    method: 'Account authorization', reauthorize: 'Authorize again', backToSetup: 'Back to login setup', reload: 'Reload account list',
    savedListUnavailable: 'The account is saved, but the list could not be loaded. Retry loading; no new login is needed.',
    expired: 'This login expired. Return to login setup to start again.',
    hint: 'Confirm on Kimi, then check authorization.', validUntil: /Valid until/,
    security: 'Confirm on Kimi only if you just started this login', openaiSecurity: /Continue on OpenAI/,
  },
} as const;

test('Kimi device login is explicit, respects poll intervals and expiry, and preserves account identity and scope', { timeout: 120_000 }, async () => {
  const server = await createIsolatedFixtureServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen(); const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const url = `http://127.0.0.1:${address.port}/e2e/fixtures/authorization-code.html?full-page&scope-controls`;
  const browser = await chromium.launch({ headless: true });
  const account = { id: 'original-kimi', tenant_external_id: 'fixture-a', driver: 'kimi-oauth', name: 'My Kimi', auth_kind: 'oauth', connection_method: 'oauth', status: 'active', credential_generation: 3, updated_at: 4, route_count: 2, config: {}, can_reauthorize: true, has_proxy: false };
  const provider = { id: 'kimi-oauth', display_name: 'Kimi', source: 'builtin', protocols: ['anthropic'], modalities: ['text'], config_schema: { type: 'object' }, credential_schema: { type: 'object', properties: { type: { const: 'oauth' } } }, oauth_adapter: { flow_kind: 'kimi_device' } };
  const initialTime = new Date('2026-09-14T12:00:00Z');
  async function open(options: { reauthorize?: boolean; locale?: keyof typeof copy; omitExpiry?: boolean } = {}) {
    const locale = options.locale ?? 'zh-CN';
    const text = copy[locale];
    const page = await browser.newPage({ viewport: { width: 390, height: 1000 } });
    page.setDefaultTimeout(5_000);
    await page.clock.install({ time: initialTime });
    await page.addInitScript((value) => localStorage.setItem('mtc-locale', value), locale);
    const writes: { path: string; body: Record<string, unknown> }[] = [];
    const state = { saved: false, failList: false, polls: 0, holdStart: false };
    let releaseStart: (() => void) | undefined;
    await page.route('**/internal/v1/**', async route => {
      const request = route.request(), path = new URL(request.url()).pathname;
      if (request.method() !== 'GET') {
        assert.equal(request.method(), 'POST');
        assert.ok(['/internal/v1/oauth/kimi/start', '/internal/v1/oauth/kimi/poll'].includes(path), 'no approval, refresh, reset or other write is allowed');
        writes.push({ path, body: request.postDataJSON() });
        if (path.endsWith('/start')) {
          if (state.holdStart) await new Promise<void>(resolve => { releaseStart = resolve; });
          return route.fulfill({ json: { driver: 'kimi-oauth', verification_url: 'https://www.kimi.com/device?user_code=MOCK-KIMI', user_code: 'MOCK-KIMI', session_token: 'mock-kimi-session', ...(options.omitExpiry ? {} : { expires_at: initialTime.getTime() + 120_000 }), poll_after_seconds: 5, security_notice: 'only_continue_if_you_started_this_login' } });
        }
        state.polls += 1;
        assert.deepEqual(request.postDataJSON(), { session_token: 'mock-kimi-session' });
        if (state.polls === 1) return route.fulfill({ status: 202, json: { status: 'pending', retry_after_seconds: 10 } });
        state.saved = true;
        return route.fulfill({ status: options.reauthorize ? 200 : 201, json: account });
      }
      if (path === '/internal/v1/provider-types') return route.fulfill({ json: [provider] });
      if (path === '/internal/v1/upstreams') return route.fulfill(state.saved && state.failList ? { status: 503, json: { error: { message: 'mock list read unavailable' } } } : { json: options.reauthorize || state.saved ? [account] : [] });
      return route.fulfill({ json: [] });
    });
    await page.goto(url);
    if (options.reauthorize) {
      await page.locator('[data-inline-edit-trigger="original-kimi"]').click();
      await page.getByRole('button', { name: text.reauthorize, exact: true }).click();
    } else {
      await page.locator('.create-journey [data-workspace-toggle]').click();
      await page.getByRole('button', { name: text.method, exact: true }).click();
    }
    return { page, writes, state, text, releaseStart: () => { assert.ok(releaseStart); releaseStart(); } };
  }
  try {
    for (const reauthorize of [false, true]) {
      const { page, writes, state, text } = await open({ reauthorize });
      assert.equal(await page.getByRole('button', { name: text.start, exact: true }).isEnabled(), true, 'Kimi permits direct login without a proxy');
      assert.equal(await page.getByRole('checkbox', { name: '使用账号网络代理' }).count(), reauthorize ? 0 : 1, 'reauthorization reuses the stored transport and never offers a proxy change');
      await page.getByRole('button', { name: text.start, exact: true }).click();
      await page.getByText('MOCK-KIMI', { exact: true }).waitFor();
      assert.deepEqual(writes[0].body, { tenant_external_id: 'fixture-a', account_name: reauthorize ? 'My Kimi' : 'Kimi', ...(reauthorize ? { upstream_account_id: 'original-kimi' } : {}) });
      const link = page.getByRole('link', { name: text.openAuthorization, exact: true });
      assert.equal(await link.getAttribute('href'), 'https://www.kimi.com/device?user_code=MOCK-KIMI');
      assert.equal(await link.getAttribute('target'), '_blank');
      assert.equal(await link.getAttribute('rel'), 'noopener noreferrer');
      assert.equal(await page.getByText(text.openaiSecurity).count(), 0, 'Kimi never shows Codex-specific copy');
      assert.match(await page.getByText(text.security, { exact: false }).first().textContent() ?? '', /Kimi/);
      await page.getByText(text.hint, { exact: false }).waitFor();
      assert.equal(await page.getByText(text.validUntil).count(), 1, 'an advertised expiry is shown as a local time');
      assert.equal(await page.getByRole('button', { name: text.countdown }).isDisabled(), true);
      await page.clock.fastForward(6_000);
      assert.equal(state.polls, 0, 'time passing never polls or approves automatically');
      await page.getByRole('button', { name: text.check, exact: true }).click();
      await page.getByRole('button', { name: '10 秒后可检查', exact: true }).waitFor();
      assert.equal(state.polls, 1);
      await page.clock.fastForward(9_000);
      assert.equal(await page.getByRole('button', { name: text.countdown }).isDisabled(), true, 'the server slow-down interval is honored');
      await page.clock.fastForward(1_000);
      state.failList = true;
      await page.getByRole('button', { name: text.check, exact: true }).click();
      await page.getByText(text.savedListUnavailable, { exact: true }).waitFor();
      assert.equal(state.polls, 2); assert.equal(writes.filter(write => write.path.endsWith('/start')).length, 1);
      state.failList = false;
      await page.getByRole('button', { name: text.reload, exact: true }).click();
      await page.getByRole('button', { name: text.reload, exact: true }).waitFor({ state: 'hidden' });
      assert.equal(state.polls, 2, 'list retry never exchanges credentials again');
      await page.close();
    }
    const expired = await open();
    await expired.page.getByRole('button', { name: expired.text.start, exact: true }).click();
    await expired.page.getByText('MOCK-KIMI', { exact: true }).waitFor();
    const screenshots = fileURLToPath(new URL('../e2e-artifacts/upstream-availability', import.meta.url)); await mkdir(screenshots, { recursive: true });
    for (const width of [390, 1440]) {
      await expired.page.setViewportSize({ width, height: 1000 });
      assert.equal(await expired.page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true, 'no horizontal overflow');
      await expired.page.screenshot({ path: `${screenshots}/kimi-device-${width}.png` });
    }
    await expired.page.clock.fastForward(121_000);
    await expired.page.getByText(expired.text.expired, { exact: true }).waitFor();
    assert.equal(await expired.page.getByText('MOCK-KIMI', { exact: true }).count(), 0);
    assert.equal(await expired.page.getByRole('link', { name: expired.text.openAuthorization, exact: true }).count(), 0);
    await expired.page.getByRole('button', { name: expired.text.backToSetup, exact: true }).click();
    assert.equal(expired.writes.length, 1, 'returning to settings does not automatically start another login');
    await expired.page.close();
    const english = await open({ locale: 'en', omitExpiry: true });
    await english.page.getByRole('button', { name: english.text.start, exact: true }).click();
    await english.page.getByText('MOCK-KIMI', { exact: true }).waitFor();
    await english.page.getByText(english.text.security, { exact: false }).first().waitFor();
    await english.page.getByText(english.text.hint, { exact: false }).waitFor();
    assert.equal(await english.page.getByText(english.text.validUntil).count(), 0, 'a missing expires_at never renders a validity time');
    assert.equal(await english.page.getByText(/Invalid Date/).count(), 0, 'a missing expires_at never renders an invalid date');
    assert.equal(await english.page.getByRole('button', { name: 'Check in 5s', exact: true }).isDisabled(), true, 'poll_after_seconds is honored without an expiry');
    await english.page.clock.fastForward(300_000);
    assert.equal(await english.page.getByText(english.text.expired).count(), 0, 'a missing expires_at never expires the login');
    assert.equal(await english.page.getByText('MOCK-KIMI', { exact: true }).count(), 1);
    await english.page.getByRole('button', { name: english.text.check, exact: true }).click();
    await english.page.getByRole('button', { name: 'Check in 10s', exact: true }).waitFor();
    await english.page.clock.fastForward(10_000);
    await english.page.getByRole('button', { name: english.text.check, exact: true }).click();
    await english.page.getByText('Upstream original-kimi is ready', { exact: true }).waitFor();
    assert.equal(english.state.polls, 2);
    await english.page.close();
    const englishExpired = await open({ locale: 'en' });
    await englishExpired.page.getByRole('button', { name: englishExpired.text.start, exact: true }).click();
    await englishExpired.page.getByText('MOCK-KIMI', { exact: true }).waitFor();
    await englishExpired.page.clock.fastForward(121_000);
    await englishExpired.page.getByText(englishExpired.text.expired, { exact: true }).waitFor();
    await englishExpired.page.getByRole('button', { name: englishExpired.text.backToSetup, exact: true }).click();
    await englishExpired.page.getByRole('button', { name: englishExpired.text.start, exact: true }).waitFor();
    assert.equal(englishExpired.writes.length, 1);
    await englishExpired.page.close();
    for (const scopeButton of ['Switch tenant', 'Switch credential']) {
      const scoped = await open(); scoped.state.holdStart = true;
      const started = scoped.page.waitForRequest(request => request.url().endsWith('/oauth/kimi/start'));
      await scoped.page.getByRole('button', { name: scoped.text.start, exact: true }).click(); await started;
      await scoped.page.getByRole('button', { name: scopeButton, exact: true }).click();
      const completed = scoped.page.waitForResponse(response => response.url().endsWith('/oauth/kimi/start'));
      scoped.releaseStart(); await completed;
      await scoped.page.locator('.create-journey [data-workspace-toggle]').click();
      await scoped.page.getByRole('button', { name: scoped.text.method, exact: true }).click();
      assert.equal(await scoped.page.getByText('MOCK-KIMI', { exact: true }).count(), 0, 'late previous-scope login cannot restore a session');
      assert.equal(scoped.writes.length, 1);
      await scoped.page.close();
    }
  } finally { await browser.close(); await server.close(); }
});
