import assert from 'node:assert/strict';
import test from 'node:test';
import { chromium } from 'playwright';

test('model create/edit forms validate and save through isolated Chromium mocks', { skip: !process.env.MTC_UX_BASE_URL }, async () => {
  const base = process.env.MTC_UX_BASE_URL!;
  assert.match(base, /^http:\/\/127\.0\.0\.1:\d+$/);
  const browser = await chromium.launch();
  try {
    const page = await browser.newPage({ viewport: { width: 1440, height: 1080 } });
    page.setDefaultTimeout(120_000);
    const requests: string[] = [];
    const errors: string[] = [];
    page.on('pageerror', (error) => errors.push(error.message));
    const account = { id: 'mock-account', tenant_external_id: 'default', name: 'Mock Codex', driver: 'openai-codex', status: 'active', config: {}, credential_generation: 1 };
    const modelRoute = { id: 'mock-route', tenant_external_id: 'default', public_model: 'public-model', upstream_model: 'gpt-mock', protocol: 'openai', priority: 0, upstream_account_ids: [account.id], enabled: true, updated_at: 1, grant_revision: 0 };
    await page.route('**/*', async (route) => {
      const url = new URL(route.request().url());
      if (url.origin !== base) return route.abort();
      if (url.pathname === '/operator') return route.fulfill({ response: await route.fetch({ url: `${base}/ui-assets/` }) });
      if (!url.pathname.startsWith('/internal/')) return route.continue();
      const method = route.request().method();
      requests.push(`${method} ${url.pathname}`);
      let body: unknown = [];
      if (url.pathname.endsWith('/tenants')) body = [{ id: 'mock', external_id: 'default', name: 'Mock tenant' }];
      else if (url.pathname.endsWith('/upstreams')) body = [account];
      else if (url.pathname.endsWith('/provider-types')) body = [{ id: 'openai-codex', display_name: 'Codex', source: 'builtin', protocols: ['openai'] }];
      else if (url.pathname.endsWith('/model-routes') || url.pathname.endsWith('/mock-route')) {
        if (method === 'GET') body = [modelRoute];
        else {
          const data = route.request().postDataJSON();
          assert.equal(data.tenant_external_id, 'default');
          assert.equal(data.upstream_model, 'gpt-mock');
          assert.equal(data.priority, 2);
          body = { ...modelRoute, ...data };
        }
      } else if (url.pathname.endsWith('/upstream-models')) body = { data: [{ id: 'gpt-mock', protocol: 'openai', complete_coverage: true, eligible_account_count: 1, supported_account_count: 1 }], eligible_account_count: 1, unknown_account_count: 0, stale_account_count: 0 };
      else if (url.pathname.endsWith('/models')) body = { status: 'ready', models: [{ id: 'gpt-mock', protocol: 'openai' }] };
      assert.equal(/\/(quota|health|sync|refresh|prepare|confirm)$/.test(url.pathname), false);
      return route.fulfill({ json: body });
    });
    await page.addInitScript(() => {
      localStorage.setItem('mtc-locale', 'en');
      localStorage.setItem('mtc.operator.service-credential.v1', 'mock-only');
      localStorage.setItem('mtc.operator.tenant.v1', 'default');
    });
    await page.goto(`${base}/operator?view=routes`);
    await page.getByRole('button', { name: 'Edit', exact: true }).first().click();
    const edit = page.locator('.inline-editor');
    await edit.getByText('1. Public model name', { exact: true }).waitFor();
    await edit.getByLabel('Priority', { exact: true }).fill('1.5');
    assert.equal(await edit.getByRole('button', { name: 'Save', exact: true }).isDisabled(), true);
    await edit.getByLabel('Priority', { exact: true }).fill('2');
    await edit.getByLabel('Upstream model', { exact: true }).click();
    await edit.getByRole('button', { name: 'Save', exact: true }).waitFor();
    await page.screenshot({ path: '/tmp/mtc-model-route-edit-desktop.png', fullPage: true });
    await edit.getByRole('button', { name: 'Save', exact: true }).click();
    await edit.waitFor({ state: 'hidden' });
    assert.ok(requests.includes('PUT /internal/v1/model-routes/mock-route'));
    const create = page.locator('details.create-resource').first();
    await create.locator('summary').click();
    await create.getByLabel(/Public model/).fill('new-public-model');
    await create.getByRole('combobox', { name: 'Specific providers' }).fill('Mock');
    await create.getByRole('option').first().click();
    await create.getByLabel('Upstream model', { exact: true }).fill('gpt-mock');
    await create.getByLabel('Priority', { exact: true }).fill('2');
    await page.setViewportSize({ width: 390, height: 844 });
    await page.screenshot({ path: '/tmp/mtc-model-route-create-mobile.png', fullPage: true });
    assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth));
    await Promise.all([
      page.waitForResponse((response) => response.url().endsWith('/internal/v1/model-routes') && response.request().method() === 'POST'),
      create.getByRole('button', { name: /Create route/ }).click(),
    ]);
    assert.ok(requests.includes('POST /internal/v1/model-routes'));
    assert.deepEqual(errors, []);
  } finally { await browser.close(); }
});
