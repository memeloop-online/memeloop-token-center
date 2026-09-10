import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

test('route model confirmation distinguishes catalog evidence from capability without upstream operations', { timeout: 60_000 }, async () => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required');
    return test.skip('Chromium is not installed');
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    for (const locale of ['en', 'zh-CN']) {
      for (const scenario of ['stale', 'unknown', 'failed', 'absent', 'partial', 'complete', 'group']) {
        const page = await browser.newPage();
        page.setDefaultTimeout(5_000);
        const requests: string[] = [];
        const errors: string[] = [];
        page.on('pageerror', (error) => errors.push(error.message));
        await page.addInitScript((value) => localStorage.setItem('mtc-locale', value), locale);
        await page.route('**/internal/**', async (route) => {
          const request = route.request();
          const url = new URL(request.url());
          requests.push(`${request.method()} ${url.pathname}`);
          assert.equal(request.method(), 'GET', 'confirmation must not sync, probe or change quota');
          assert.ok(url.pathname === '/internal/v1/upstream-models' || /^\/internal\/v1\/upstreams\/[ab]\/models$/.test(url.pathname));
          const model = url.searchParams.get('q') ?? 'gpt-5.6-luna';
          await route.fulfill({
            status: scenario === 'failed' ? 503 : 200, contentType: 'application/json',
            body: JSON.stringify({
              data: scenario === 'absent' ? [] : [{
                id: model, protocol: 'openai', supported_account_count: scenario === 'complete' ? 2 : 1,
                eligible_account_count: 2, complete_coverage: scenario === 'complete',
              }],
              eligible_account_count: 2, unknown_account_count: scenario === 'unknown' ? 1 : 0,
              stale_account_count: ['stale', 'group'].includes(scenario) ? 1 : 0,
            }),
          });
        });
        await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/route-model-validation.html?scenario=${scenario}`);
        await page.waitForResponse((response) => response.url().includes('/internal/v1/upstream-models?'));
        const save = page.getByRole('button', { name: 'Save route', exact: true });
        const checkbox = page.getByRole('checkbox');
        if (scenario === 'complete') {
          await page.waitForFunction(() => !document.querySelector<HTMLButtonElement>('main > button')?.disabled);
          assert.equal(await checkbox.count(), 0);
          await save.click();
          assert.equal(JSON.parse(await page.locator('[data-submitted]').textContent() ?? '{}').custom_model_confirmed, false);
        } else if (scenario === 'group') {
          assert.equal(await checkbox.count(), 0);
          assert.equal(await save.isDisabled(), true);
        } else {
          await checkbox.waitFor();
          assert.equal(await checkbox.count(), 1, `${locale}/${scenario} must offer exactly one confirmation`);
          assert.equal(await save.isDisabled(), true);
          if (scenario !== 'partial') {
            assert.match(await page.locator('.notice').textContent() ?? '', /Codex OAuth/);
            assert.equal(await page.getByText(locale === 'en' ? /Sync models and confirm complete/ : /请先同步模型/).count(), 0);
          }
          await checkbox.check();
          await save.click();
          const payload = JSON.parse(await page.locator('[data-submitted]').textContent() ?? '{}');
          assert.equal(payload.custom_model_confirmed, scenario !== 'partial');
          assert.deepEqual(payload.upstream_account_ids, ['a', 'b']);
          if (scenario === 'stale') {
            await page.getByRole('button', { name: 'Change model', exact: true }).click();
            await page.waitForFunction(() => document.querySelector<HTMLInputElement>('input[type=checkbox]')?.checked === false);
            assert.equal(await save.isDisabled(), true, 'changing Luna to Terra must require a new confirmation');
            await checkbox.check();
            await page.getByRole('button', { name: 'Change candidates', exact: true }).click();
            await page.waitForFunction(() => document.querySelector<HTMLInputElement>('input[type=checkbox]')?.checked === false);
            assert.equal(await save.isDisabled(), true, 'changing explicit accounts must invalidate confirmation');
          }
        }
        assert.ok(requests.length > 0);
        assert.deepEqual(errors, []);
        await page.close();
      }
    }
  } finally {
    await browser.close();
    await server.close();
  }
});
