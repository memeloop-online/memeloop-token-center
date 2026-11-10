import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

const endpoint = '/internal/v1/plugins/mtc-health-intelligence/private-data/concurrency-advice';
function ready(tenant = 'tenant-a', stale = false, series: unknown[] = [{
  dimension: 'account', identity: 'Example account', current: 0, sampled_peak: 4, status: 'manual_review',
  sampling: { largestBucketSamples: 12, targetSamplesPerBucket: 20, sampleProgressInBestBucket: 0.6, windowProgressInBestBucket: 0.5 },
  recommendation: { kind: 'observed_range_for_manual_review', concurrencyRange: { from: 2, to: 4 } },
}]) {
  return { state: 'ready', data: { contract_version: 'health_concurrency_advice_v1', application_mode: 'manual_only',
    tenant_external_id: tenant, stale, series } };
}

test('private observations hide absent data and preserve scoped ready, stale and retry flows', { timeout: 90_000 }, async () => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    return test.skip('Chromium required');
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false,
    logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const base = `http://127.0.0.1:${address.port}`;
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 390, height: 900 } });
    const errors: string[] = [];
    const methods: string[] = [];
    const tenants: string[] = [];
    page.on('pageerror', error => errors.push(error.message));
    let body: unknown = { state: 'not_configured' };
    let release: (() => void) | undefined;
    let gate: Promise<void> | undefined;
    await page.route('**/*', async route => {
      const url = new URL(route.request().url());
      if (url.origin !== base) return route.abort();
      if (url.pathname !== endpoint) return route.continue();
      methods.push(route.request().method()); tenants.push(url.searchParams.get('tenant_external_id') ?? '');
      const response = body;
      if (gate) await gate;
      await route.fulfill({ json: response, headers: { 'Cache-Control': 'private, no-store' } });
    });
    const panel = page.getByRole('region', { name: '并发观测', exact: true });
    const settle = () => page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
    const open = async (value: unknown) => {
      body = value;
      const response = page.waitForResponse(r => new URL(r.url()).pathname === endpoint);
      await page.goto(`${base}/e2e/fixtures/plugin-concurrency-advice.html`);
      await response; await settle();
    };
    gate = new Promise<void>(resolve => { release = resolve; });
    await page.goto(`${base}/e2e/fixtures/plugin-concurrency-advice.html`);
    await page.getByRole('heading', { name: 'Private observation fixture' }).waitFor();
    assert.equal(await panel.count(), 0, 'loading adds no empty panel');
    const first = page.waitForResponse(r => new URL(r.url()).pathname === endpoint);
    release!(); gate = undefined; await first; await settle();
    assert.equal(await panel.count(), 0);
    for (const value of [{ state: 'select_tenant' }, { state: 'permission_required' }, ready('tenant-a', false, [])]) {
      await open(value); assert.equal(await panel.count(), 0);
    }
    await open(ready()); await panel.getByRole('heading', { name: '账号 · Example account' }).waitFor();
    assert.match(await panel.innerText(), /0 \/ 4/);
    assert.match(await panel.innerText(), /2–4/);
    await panel.getByText('证据详情', { exact: true }).click();
    assert.match(await panel.innerText(), /observed_range_for_manual_review/);
    await open(ready('tenant-a', true));
    assert.match(await panel.innerText(), /当前显示上次观测结果/);
    assert.doesNotMatch(await panel.innerText(), /2–4/);
    for (const value of [{ state: 'unavailable' }, { state: 'ready', data: {} }, ready('other-tenant')]) {
      await open(value);
      await panel.getByRole('status').waitFor();
      assert.match(await panel.innerText(), /请稍后刷新/);
      assert.equal(await panel.getByRole('heading', { level: 3 }).count(), 0);
      assert.doesNotMatch(await panel.innerText(), /请求负载|置信区间/);
    }
    body = ready();
    const refreshed = page.waitForResponse(r => new URL(r.url()).pathname === endpoint);
    await panel.getByRole('button', { name: '刷新', exact: true }).click();
    await refreshed; await panel.getByRole('heading', { name: '账号 · Example account' }).waitFor();
    body = { state: 'not_configured' };
    gate = new Promise<void>(resolve => { release = resolve; });
    await page.getByRole('button', { name: 'Switch tenant' }).click();
    await panel.waitFor({ state: 'hidden' });
    const switched = page.waitForResponse(r => new URL(r.url()).pathname === endpoint);
    release!(); gate = undefined; await switched; await settle();
    assert.equal(await panel.count(), 0, 'prior tenant observations remain hidden');
    assert.equal(tenants.at(-1), 'tenant-b');
    body = ready();
    const englishRead = page.waitForResponse(r => new URL(r.url()).pathname === endpoint);
    await page.goto(`${base}/e2e/fixtures/plugin-concurrency-advice.html?locale=en`);
    await englishRead;
    const english = page.getByRole('region', { name: 'Concurrency observations', exact: true });
    await english.getByRole('heading', { name: 'Account · Example account' }).waitFor();
    assert.match(await english.innerText(), /Ranges describe the current sample/);
    assert.doesNotMatch(await english.innerText(), /do not|not a proven optimum/);
    assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth));
    assert(methods.every(method => method === 'GET'));
    assert.deepEqual(errors, []);
  } finally { await browser.close(); await server.close(); }
});
