import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

declare global {
  interface Window {
    formFixture: {
      writes: Array<{ path: string; body: Record<string, unknown> }>;
      reads: string[]; holdCatalog: boolean; finish: () => void;
      changeAndSubmit: (change: () => void) => void;
    };
  }
}

test('credential and route forms group fields, stay within the viewport and guard submission', { timeout: 60_000 }, async () => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium is required for management form contracts');
    return test.skip('Chromium is not installed');
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const address = server.httpServer?.address();
  assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    page.setDefaultTimeout(5000);
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    const errors: string[] = [];
    page.on('pageerror', error => errors.push(error.message));
    for (const view of ['credentials', 'services', 'routes']) {
      await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/management-forms.html?view=${view}`);
      const create = page.locator('.create-resource');
      await create.locator(':scope > summary').click();
      await create.locator('input').first().waitFor();
      for (const theme of ['light', 'dark']) {
        await page.evaluate(theme => { document.documentElement.dataset.theme = theme; }, theme);
        for (const width of [320, 390, 768, 1024, 1440, 1920, 2560]) {
          await page.setViewportSize({ width, height: 1000 });
          assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false, `${view} ${theme} ${width}`);
        }
      }
      if (view === 'credentials') {
        assert.deepEqual(await create.locator('.management-form-section > legend').allTextContents(), [
          '1 · Credential identity', '2 · Model access', '3 · Balance and currency', '4 · Policy and limits',
        ]);
        assert.equal(await create.locator('[id$="_route_ids"],[id$="_route_group_ids"]').count(), 0, 'raw UUID arrays must not duplicate the authoritative pickers');
        await create.getByRole('button', { name: 'Create credential', exact: true }).click();
        await create.locator('.schema-errors').waitFor();
        assert.equal(await create.getByLabel('Credential alias', { exact: false }).evaluate(element => document.activeElement === element), true);
        await create.getByLabel('Credential alias', { exact: false }).fill('Synthetic client');
        await create.getByLabel('Principal', { exact: false }).fill('synthetic-principal');
        await create.locator('form').evaluate(form => {
          (form as HTMLFormElement).requestSubmit();
          (form as HTMLFormElement).requestSubmit();
        });
        await page.waitForFunction(() => window.formFixture.writes.length === 1);
        assert.equal(await create.locator('[aria-busy=true]').count(), 1);
        const write = await page.evaluate(() => window.formFixture.writes[0]);
        assert.equal(write.body.tenant_external_id, 'alpha');
        assert.equal(write.body.currency, 'USD');
        assert.equal(write.body.initial_balance, '0');
        await page.getByRole('button', { name: 'Switch tenant' }).click();
        await page.evaluate(() => window.formFixture.finish());
        assert.equal(await page.getByText('fixture-original-key', { exact: true }).count(), 0, 'old-scope secrets must never appear in a new scope');
      } else if (view === 'services') {
        const scopes = create.getByRole('combobox');
        await scopes.fill('keys:read');
        await scopes.press('ArrowDown');
        await scopes.press('Enter');
        assert.equal(await create.locator('.selection-chip').count(), 1);
        await scopes.fill('no-match');
        await scopes.press('Enter');
        assert.equal(await page.evaluate(() => window.formFixture.writes.length), 0, 'autocomplete Enter cannot submit');
        await scopes.press('Escape');
        assert.equal(await scopes.getAttribute('aria-expanded'), 'false');
      } else {
        const upstream = create.getByRole('combobox', { name: 'Specific providers', exact: true });
        await page.evaluate(() => { document.documentElement.dataset.theme = 'light'; });
        await upstream.fill('Fixture Provider');
        await create.getByRole('group', { name: 'Fixture Provider', exact: true }).waitFor();
        const idleOption = create.locator('.combobox-popover button:not(.active)').first();
        assert.equal(await idleOption.isVisible(), true, 'contrast must be measured in an actually open light popover');
        const contrast = await idleOption.evaluate(option => {
          const rgb = (color: string) => color.match(/[\d.]+/g)!.slice(0, 3).map(Number);
          const luminance = (color: string) => rgb(color).map(value => {
            const channel = value / 255;
            return channel <= 0.04045 ? channel / 12.92 : ((channel + 0.055) / 1.055) ** 2.4;
          }).reduce((sum, channel, index) => sum + channel * [0.2126, 0.7152, 0.0722][index], 0);
          const background = luminance(getComputedStyle(option.closest('.combobox-popover')!).backgroundColor);
          return Math.min(...[option, ...option.querySelectorAll('span,small')].map(element => {
            const foreground = luminance(getComputedStyle(element).color);
            return (Math.max(foreground, background) + 0.05) / (Math.min(foreground, background) + 0.05);
          }));
        });
        assert.ok(contrast >= 4.5, `idle option contrast ${contrast} must meet WCAG AA`);
        await upstream.press('ArrowDown');
        await upstream.press('Enter');
        await create.getByLabel('Public model', { exact: true }).fill('client-model');
        const model = create.getByRole('combobox', { name: 'Upstream model', exact: true });
        const submit = create.getByRole('button', { name: 'Create route', exact: true });
        await model.fill('fixture-model');
        await page.waitForFunction(() => !document.querySelector<HTMLButtonElement>('.create-resource button[type=submit]')!.disabled);
        await create.locator('.shared-model-popover input').fill('Fixture Provider');
        await create.locator('.shared-model-popover').getByRole('group', { name: 'Fixture Provider', exact: true }).waitFor();
        assert.equal(await model.inputValue(), 'fixture-model', 'display-name search does not change the selected model');
        assert.equal(await create.locator('.shared-model-popover [aria-modal=true]').count(), 0);
        // Change the candidate pool and submit in one browser task, before a
        // replacement aggregate response can make the new scope valid.
        await upstream.fill('Backup account');
        await create.getByRole('option', { name: /Backup account/ }).waitFor();
        await page.evaluate(() => {
          window.formFixture.holdCatalog = true;
          window.formFixture.changeAndSubmit(() => {
            [...document.querySelectorAll<HTMLButtonElement>('.create-resource .combobox-popover button')]
              .find(button => button.textContent?.includes('Backup account'))!.click();
          });
        });
        assert.equal(await submit.isDisabled(), true, 'previous full coverage cannot validate a different candidate pool');
        assert.equal(await page.evaluate(() => window.formFixture.writes.length), 0);
        await model.fill('custom-one');
        await create.locator('.custom-model-confirm input').check();
        assert.equal(await submit.isDisabled(), false);
        await model.evaluate(input => {
          window.formFixture.changeAndSubmit(() => {
            Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(input, 'custom-two');
            input.dispatchEvent(new Event('input', { bubbles: true }));
          });
        });
        assert.equal(await submit.isDisabled(), true, 'allowCustom cannot be reused for another model');
        assert.equal(await create.locator('.custom-model-confirm input').isChecked(), false);
        assert.equal(await page.evaluate(() => window.formFixture.writes.length), 0);
        await create.getByLabel('Priority', { exact: false }).fill('1.5');
        assert.equal(await submit.isDisabled(), true);
      }
    }
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/management-forms.html?view=large-catalog`);
    const largeModel = page.getByRole('combobox', { name: 'Upstream model', exact: true });
    await largeModel.click();
    await page.locator('.shared-model-popover').getByRole('group', { name: 'Fixture Provider', exact: true }).waitFor();
    await page.waitForFunction(() => !document.querySelector('.shared-model-popover [role=status]'));
    const accountReads = await page.evaluate(() => window.formFixture.reads.filter(path => /\/upstreams\/[^/]+\/models\?/.test(path)).length);
    assert.equal(accountReads, 8, '500 candidates must never cause 500 account catalog GETs');
    assert.ok(await page.locator('.shared-model-popover').getByRole('group', { name: 'Source not provided', exact: true }).count() > 0,
      'uninspected support must remain explicitly unknown after bounded loading finishes');
    assert.equal(await largeModel.getAttribute('aria-invalid'), 'false');
    await page.evaluate(() => {
      window.formFixture.holdCatalog = true;
      [...document.querySelectorAll<HTMLButtonElement>('button')].find(button => button.textContent === 'Change group membership')!.click();
    });
    await page.waitForFunction(() => document.querySelector('.shared-model-picker > input')?.getAttribute('aria-invalid') === 'true');
    assert.equal(await largeModel.getAttribute('aria-invalid'), 'true', 'unchanged group IDs cannot reuse coverage after membership changes');
    assert.deepEqual(errors, []);
  } finally { await browser.close(); await server.close(); }
});
