import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createIsolatedFixtureServer as createServer } from './support/isolated-vite-server.js';

test('Operator plugin page installs, explicitly reviews, publishes and rolls back despite broken active catalog', { timeout: 60_000 }, async context => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    context.skip('Chromium not installed'); return;
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer!.address();
  assert.ok(address && typeof address === 'object');
  const browser = await chromium.launch({ headless: true });
  const page = await browser.newPage();
  let releaseInitialHistory: () => void = () => undefined;
  const initialHistoryGate = new Promise<void>((resolve) => { releaseInitialHistory = resolve; });
  let holdInitialHistory = true;
  // The application defaults to Chinese. This contract uses English labels,
  // so establish the locale before I18nProvider reads persisted preferences.
  await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
  try {
    await page.route('**/internal/v1/plugins/runtime-access', async route => {
      assert.equal(route.request().headers().authorization, 'Bearer operator-test');
      await route.fulfill({ json: { can_view_runtime: true, can_manage_runtime: true } });
    });
    let current = { revision: 1, inventory_id: 'baseline', reason: 'initial' };
    const revisions = [current];
    const candidates = [{ inventory_id: 'baseline', staged: true, plugins: {} as Record<string, string[]> }];
    let job: { id: string; inventory_id: string; actor: string; status: string; packages: string[]; completed_packages: number; review_digest: string; review: unknown; failure_category: null; created_at: number; updated_at: number } | undefined;
    const writes: { path: string; body: any; key?: string }[] = [];
    await page.route('**/internal/v1/plugin-runtime**', async route => {
      const request = route.request();
      assert.equal(request.headers().authorization, 'Bearer operator-test');
      const path = new URL(request.url()).pathname;
      if (request.method() === 'GET') {
        if (path.endsWith('/history')) {
          if (holdInitialHistory) {
            await initialHistoryGate;
            holdInitialHistory = false;
          }
          await route.fulfill({ json: { runtime_enabled: true, installation_enabled: true, revisions, installations: job ? [{ ...job, review: null }] : [], audit: [] } });
        }
        else if (path.endsWith('/installations/job')) await route.fulfill({ json: job });
        else await route.fulfill({ json: { current, candidates } });
        return;
      }
      const body = request.postDataJSON();
      writes.push({ path, body, key: request.headers()['idempotency-key'] });
      if (path.endsWith('/installations')) {
        job = { id: 'job', inventory_id: body.inventory_id, packages: body.packages, completed_packages: 1, actor: 'service:operator', status: 'review', review_digest: 'review-exact', review: { plugins: [{ id: 'new-plugin', version: '1.0.0', wit_version: '0.2.0', capabilities: [{ kind: 'http', allowed_origins: ['https://api.example.com'] }], contributions: {} }] }, failure_category: null, created_at: 1, updated_at: 2 };
        await route.fulfill({ status: 202, json: job });
      } else if (path.endsWith('/approve')) {
        assert.equal(body.review_digest, 'review-exact');
        job!.status = 'registered';
        candidates.push({ inventory_id: 'new-inventory', staged: true, plugins: { 'new-plugin': ['1.0.0'] } });
        await route.fulfill({ status: 204 });
      } else if (path.endsWith('/publish')) {
        assert.equal(body.expected_revision, 1);
        current = { revision: 2, inventory_id: body.inventory_id, reason: 'reload' };
        revisions.unshift(current);
        await route.fulfill({ json: current });
      } else if (path.endsWith('/rollback')) {
        assert.deepEqual(body, { target_revision: 1, expected_revision: 2 });
        current = { revision: 3, inventory_id: 'baseline', reason: 'rollback' };
        revisions.unshift(current);
        await route.fulfill({ json: current });
      } else await route.fulfill({ status: 204 });
    });
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/plugin-runtime.html`);
    await page.getByText('Current revision: 1 · baseline', { exact: true }).waitFor();
    await page.getByRole('heading', { name: 'Available inventories', exact: true }).waitFor();
    assert.equal(await page.getByLabel('New inventory ID').count(), 0, 'history-only controls remain pending without hiding current runtime state');
    releaseInitialHistory();
    await page.getByText('broken current catalog', { exact: false }).waitFor();
    await page.getByLabel('New inventory ID').fill('new-inventory');
    const reference = `ghcr.io/example/new@sha256:${'a'.repeat(64)}`;
    await page.getByLabel('Digest-pinned OCI references (one per line)').fill(reference);
    await page.getByRole('button', { name: 'Install for review' }).click();
    await page.getByRole('button', { name: 'Tasks and version records' }).click();
    const approve = page.getByRole('button', { name: 'Approve this exact inventory', exact: true });
    await approve.waitFor();
    assert.equal(await approve.isDisabled(), true);
    await page.getByRole('button', { name: 'Review manifest and requested capabilities' }).click();
    await page.getByText('https://api.example.com', { exact: false }).waitFor();
    await page.getByRole('checkbox', { name: 'Approve this exact inventory' }).check();
    await approve.click();
    await page.getByText('registered', { exact: true }).waitFor();
    await page.getByRole('checkbox', { name: 'Confirm global activation' }).check();
    await page.getByRole('button', { name: 'Publish inventory', exact: true }).last().click();
    await page.getByTestId('reloads').filter({ hasText: '1' }).waitFor();
    await page.getByRole('checkbox', { name: 'Confirm global activation' }).check();
    await page.getByRole('button', { name: 'Roll back to this version', exact: true }).last().click();
    await page.getByTestId('reloads').filter({ hasText: '2' }).waitFor();
    assert.deepEqual(writes[0].body, { inventory_id: 'new-inventory', packages: [reference] });
    assert.ok(writes.every((write) => typeof write.key === 'string' && write.key.length > 0));
    assert.deepEqual(writes.map((write) => write.path), ['/internal/v1/plugin-runtime/installations', '/internal/v1/plugin-runtime/installations/job/approve', '/internal/v1/plugin-runtime/publish', '/internal/v1/plugin-runtime/rollback']);
    // Status and history are independent reads. A status failure must not hide
    // installation, review or durable history that loaded successfully.
    job!.status = 'review';
    const failStatus = (route: import('playwright').Route) => route.fulfill({ status: 503, json: { error: { message: 'runtime status unavailable' } } });
    await page.route('**/internal/v1/plugin-runtime', failStatus);
    await page.reload();
    await page.getByText('runtime status unavailable', { exact: false }).waitFor();
    await page.getByLabel('New inventory ID').waitFor();
    await page.getByRole('button', { name: 'Tasks and version records' }).click();
    await page.getByRole('heading', { name: 'Version history', exact: true }).waitFor();
    await page.getByRole('button', { name: 'Review manifest and requested capabilities' }).waitFor();
    assert.equal(await page.getByRole('checkbox', { name: 'Approve this exact inventory' }).count(), 1);
    await page.getByText('baseline', { exact: false }).first().waitFor();
    await page.unroute('**/internal/v1/plugin-runtime', failStatus);
    // A global reader can inspect history but cannot publish or roll back.
    await page.route('**/internal/v1/plugins/runtime-access', route => route.fulfill({ json: { can_view_runtime: true, can_manage_runtime: false } }));
    await page.reload();
    await page.getByRole('button', { name: 'Tasks and version records' }).click();
    await page.getByRole('heading', { name: 'Version history', exact: true }).waitFor();
    assert.equal(await page.getByRole('button', { name: 'Install for review' }).count(), 0);
    assert.equal(await page.getByRole('button', { name: 'Publish inventory', exact: true }).first().isDisabled(), true);
    assert.equal(await page.getByRole('button', { name: 'Roll back to this version', exact: true }).first().isDisabled(), true);
    // Tenant principals must never issue a global-runtime request, not merely
    // accept its 403. Capability lookup remains a successful self-scope read.
    let tenantRuntimeReads = 0;
    page.on('request', request => {
      if (new URL(request.url()).pathname.startsWith('/internal/v1/plugin-runtime')) tenantRuntimeReads++;
    });
    await page.route('**/internal/v1/plugins/runtime-access', route => route.fulfill({ json: { can_view_runtime: false, can_manage_runtime: false } }));
    const accessResponse = page.waitForResponse(response => new URL(response.url()).pathname === '/internal/v1/plugins/runtime-access');
    await page.reload();
    await accessResponse;
    await page.getByText('broken current catalog', { exact: false }).waitFor();
    await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
    assert.equal(await page.getByRole('heading', { name: 'Plugin installation and versions' }).count(), 0);
    assert.equal(tenantRuntimeReads, 0);
  } finally { releaseInitialHistory(); await browser.close(); await server.close(); }
});
