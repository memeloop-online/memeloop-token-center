import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { mkdir } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

declare global { interface Window { semanticAtomReads: number; semanticPageReads: number; semanticPageRequest: string; semanticContentReads: number; semanticContentCursors: string[] } }

test('semantic viewer reads verified atoms lazily, preserves parts and pages without resident history growth', { timeout: 45_000 }, async () => {
  if (!existsSync(chromium.executablePath())) {
    if (process.env.MTC_REQUIRE_BROWSER === '1') throw new Error('Chromium required');
    return test.skip('Chromium required');
  }
  const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), configFile: false, logLevel: 'silent', server: { host: '127.0.0.1', port: 0 } });
  await server.listen(); const address = server.httpServer?.address(); assert.ok(address && typeof address !== 'string');
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 390, height: 900 } });
    await page.addInitScript(() => localStorage.setItem('mtc-locale', 'en'));
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/semantic-session.html`);
    await page.getByText('Verified latest answer', { exact: true }).waitFor();
    assert.equal(await page.locator('[data-semantic-role=assistant]').count(), 1);
    assert.equal(await page.locator('[data-semantic-role=agent]').count(), 0);
    assert.equal(await page.evaluate(() => window.semanticAtomReads), 1);
    await page.getByText('tool_result_not_recorded', { exact: true }).waitFor();
    await page.getByText('Verified latest answer', { exact: true }).waitFor();
    await page.getByRole('button', { name: 'Reasoning', exact: true }).click();
    await page.getByText('Separate reasoning', { exact: true }).waitFor();
    await page.getByRole('button', { name: 'Tool call · lookup', exact: true }).click();
    assert.match(await page.locator('.semantic-atom-content').innerText(), /"query": "structured"/);
    assert.equal(await page.evaluate(() => window.semanticAtomReads), 1);
    await page.getByRole('button', { name: 'Refresh conversation', exact: true }).click();
    await page.waitForFunction(() => window.semanticPageReads === 2);
    await page.getByText('Verified latest answer', { exact: true }).waitFor();
    assert.equal(await page.evaluate(() => window.semanticAtomReads), 1, 'same revision preserves the open atom');
    const artifacts = fileURLToPath(new URL('../e2e-artifacts/semantic-viewer/', import.meta.url));
    await mkdir(artifacts, { recursive: true });
    for (const theme of ['light', 'dark']) {
      await page.evaluate(value => { document.documentElement.dataset.theme = value; }, theme);
      await page.locator(`[data-semantic-theme=${theme}]`).waitFor();
      for (const width of [390, 1440]) {
        await page.setViewportSize({ width, height: 900 });
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
        await page.screenshot({ path: join(artifacts, `semantic-${theme}-${width}.png`), fullPage: true });
      }
    }
    await page.setViewportSize({ width: 390, height: 900 });
    await page.getByRole('button', { name: 'Earlier messages', exact: true }).click();
    await page.getByText('Verified earlier answer', { exact: true }).waitFor();
    assert.equal(await page.getByText('Verified latest answer', { exact: true }).count(), 0, 'old resident window is released');
    assert.equal(await page.locator('[data-semantic-role=user]').count(), 1);
    await page.getByText('Verified earlier answer', { exact: true }).waitFor();
    assert.equal(await page.evaluate(() => window.semanticAtomReads), 2);
    await page.getByRole('button', { name: 'Newer messages', exact: true }).click();
    await page.getByText('Verified latest answer', { exact: true }).waitFor();
    assert.equal(await page.evaluate(() => window.semanticAtomReads), 3);
    assert.equal(await page.locator('.semantic-atom-content').count(), 1);
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
    assert.equal(await page.getByRole('textbox').count(), 0, 'read-only viewer has no composer');
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/semantic-session.html?scenario=unknown`);
    await page.getByText('Verified latest answer', { exact: true }).waitFor();
    assert.equal(await page.locator('[data-semantic-role=agent]').count(), 0);
    assert.equal(await page.locator('[data-semantic-role=unrecorded]').count(), 1);
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/semantic-session.html?scenario=many`);
    for (let step = 0; step < 10; step++) {
      await page.locator('[data-semantic-ordinal="32"]').scrollIntoViewIfNeeded();
      if (await page.getByText('Message item-32', { exact: true }).isVisible()) break;
      await page.waitForTimeout(100);
    }
    await page.getByText('Message item-32', { exact: true }).waitFor({ timeout: 5000 }).catch(async error => {
      console.log('semantic-many-state', await page.locator('[data-testid=conversation-viewport]').evaluate(node => ({ scroll: node.scrollTop, height: node.clientHeight, total: node.scrollHeight, text: node.textContent?.slice(-4000), children: [...node.querySelectorAll('[data-semantic-ordinal]')].map(item => ({ ordinal: item.getAttribute('data-semantic-ordinal'), rect: item.getBoundingClientRect().toJSON(), text: item.textContent?.slice(0, 120) })) })));
      throw error;
    });
    assert.ok(await page.locator('[data-semantic-role=assistant]').count() <= 32);
    await page.getByRole('button', { name: 'Earlier messages', exact: true }).click();
    await page.getByText('Message item-0', { exact: true }).waitFor();
    await page.getByRole('button', { name: 'Newer messages', exact: true }).click();
    for (let step = 0; step < 10; step++) {
      await page.locator('[data-semantic-ordinal="32"]').scrollIntoViewIfNeeded();
      if (await page.getByText('Message item-32', { exact: true }).isVisible()) break;
      await page.waitForTimeout(100);
    }
    await page.getByText('Message item-32', { exact: true }).waitFor({ timeout: 5000 }).catch(async error => {
      console.log('semantic-many-state', await page.locator('[data-testid=conversation-viewport]').evaluate(node => ({ scroll: node.scrollTop, height: node.clientHeight, total: node.scrollHeight, text: node.textContent?.slice(-4000), children: [...node.querySelectorAll('[data-semantic-ordinal]')].map(item => ({ ordinal: item.getAttribute('data-semantic-ordinal'), rect: item.getBoundingClientRect().toJSON(), text: item.textContent?.slice(0, 120) })) })));
      throw error;
    });
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/semantic-session.html?scenario=large`);
    const field = page.locator('[data-semantic-field=text]').first();
    await field.locator('[data-content-index="0"]').waitFor();
    for (let index = 1; index <= 5; index++) {
      await field.evaluate(node => { node.scrollTop = node.scrollHeight; node.dispatchEvent(new WheelEvent('wheel', { deltaY: 300, bubbles: true })); });
      await field.locator(`[data-content-index="${index}"]`).waitFor({ state: 'attached' });
      assert.ok(await field.locator('[data-content-index]').count() <= 3);
    }
    await field.evaluate(node => { node.scrollTop = node.scrollHeight; node.dispatchEvent(new WheelEvent('wheel', { deltaY: 300, bubbles: true })); });
    await page.getByText('COMPLETE LARGE FIELD', { exact: true }).waitFor();
    for (let index = 2; index >= 0; index--) {
      await field.evaluate(node => { node.scrollTop = 0; node.dispatchEvent(new WheelEvent('wheel', { deltaY: -300, bubbles: true })); });
      await field.locator(`[data-content-index="${index}"]`).waitFor({ state: 'attached' });
    }
    for (const [title, name] of [['Tool call · lookup', 'arguments'], ['Tool result · lookup', 'result_content']]) {
      await page.locator(`[data-semantic-part-index="${name === 'arguments' ? 2 : 3}"]`).scrollIntoViewIfNeeded();
      await page.getByRole('button', { name: title, exact: true }).click();
      const toolField = page.locator(`[data-semantic-field=${name}]`);
      await toolField.locator('[data-content-index="0"]').waitFor();
      for (let index = 1; index <= 5; index++) {
        await toolField.evaluate(node => { node.scrollTop = node.scrollHeight; node.dispatchEvent(new WheelEvent('wheel', { deltaY: 300, bubbles: true })); });
        await toolField.locator(`[data-content-index="${index}"]`).waitFor({ state: 'attached' });
        assert.ok(await toolField.locator('[data-content-index]').count() <= 3);
      }
      await toolField.evaluate(node => { node.scrollTop = node.scrollHeight; });
      await toolField.getByText('COMPLETE LARGE FIELD', { exact: true }).waitFor();
    }
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/semantic-session.html?scenario=revision`);
    await page.getByText('Error: Semantic content identity or offset changed; refresh the conversation', { exact: true }).waitFor();
    assert.equal(await page.getByText('Stale content', { exact: true }).count(), 0);
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/semantic-session.html?scenario=preview`);
    await page.getByText('😀首窗', { exact: true }).waitFor();
    assert.equal(await page.evaluate(() => window.semanticContentReads), 0, 'verified preview needs no extra content request');
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/semantic-session.html?scenario=preview-partial`);
    await page.getByText('😀首窗', { exact: true }).waitFor();
    assert.equal(await page.evaluate(() => window.semanticContentReads), 0);
    await page.getByRole('button', { name: 'More content', exact: true }).click();
    await page.getByText('tail after preview', { exact: false }).waitFor();
    assert.equal(await page.evaluate(() => window.semanticContentReads), 1, 'partial preview resumes at the UTF8 cursor');
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/semantic-session.html?scenario=preview-partial-large`);
    const previewField = page.locator('[data-semantic-field=text]').first();
    await page.getByText('😀首窗', { exact: true }).waitFor();
    for (let index = 1; index <= 5; index++) {
      await previewField.evaluate(node => { node.scrollTop = node.scrollHeight; node.dispatchEvent(new WheelEvent('wheel', { deltaY: 300, bubbles: true })); });
      await previewField.locator(`[data-content-index="${index}"]`).waitFor({ state: 'attached' });
    }
    for (let index = 2; index >= 0; index--) {
      await previewField.evaluate(node => { node.scrollTop = 0; node.dispatchEvent(new WheelEvent('wheel', { deltaY: -300, bubbles: true })); });
      await previewField.locator(`[data-content-index="${index}"]`).waitFor({ state: 'attached' });
    }
    assert.equal(await previewField.locator('[data-content-index="0"]').textContent(), '😀首窗');
    for (let index = 3; index <= 5; index++) {
      await previewField.evaluate(node => { node.scrollTop = node.scrollHeight; node.dispatchEvent(new WheelEvent('wheel', { deltaY: 300, bubbles: true })); });
      await previewField.locator(`[data-content-index="${index}"]`).waitFor({ state: 'attached' });
    }
    const resumed = await page.evaluate(() => window.semanticContentCursors);
    assert.ok(resumed.every(cursor => !cursor.endsWith(':text')), 'returning to the shared preview must not request a different chunk at offset zero');
    assert.equal(await page.getByRole('alert').count(), 0);
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/semantic-session.html?scenario=tabs`);
    const requestsTab = page.getByRole('tab', { name: 'Requests', exact: true });
    await requestsTab.focus();
    await requestsTab.press('ArrowRight');
    const generationsTab = page.getByRole('tab', { name: 'Generations', exact: true });
    assert.equal(await generationsTab.evaluate(node => document.activeElement === node), true);
    await generationsTab.press('End');
    const sessionsTab = page.getByRole('tab', { name: 'Sessions', exact: true });
    assert.equal(await sessionsTab.evaluate(node => document.activeElement === node), true);
    await sessionsTab.press('Enter');
    assert.equal(await sessionsTab.getAttribute('aria-selected'), 'true');
    assert.equal(await requestsTab.getAttribute('aria-selected'), 'false');
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/semantic-session.html?scenario=scope`);
    await page.waitForFunction(() => window.semanticPageRequest === 'request-a');
    await page.getByRole('button', { name: 'Open another session', exact: true }).click();
    await page.waitForFunction(() => window.semanticPageRequest === 'request-b');
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/semantic-session.html?scenario=error`);
    await page.getByText('Semantic read HTTP 500', { exact: true }).waitFor();
    assert.equal(await page.getByText('semantic_publication_pending', { exact: true }).count(), 0);
    assert.equal(await page.getByText('The semantic record is not fully published.', { exact: true }).count(), 0);
  } finally { await browser.close(); await server.close(); }
});
