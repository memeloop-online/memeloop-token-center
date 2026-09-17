import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium, type Page } from 'playwright';
import { createServer } from 'vite';

test('analytics background exposes actual indexed buckets through pointer, keyboard and touch', { timeout: 30000 }, async context => {
  const executablePath=process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH??chromium.executablePath();
  if(!existsSync(executablePath)){if(process.env.MTC_REQUIRE_BROWSER==='1')throw new Error('Chromium required');return context.skip('Chromium required');}
  const server=await createServer({root:fileURLToPath(new URL('..',import.meta.url)),configFile:false,logLevel:'silent',server:{host:'127.0.0.1',port:0}});await server.listen();
  const address=server.httpServer!.address();assert.ok(address&&typeof address!=='string');
  const browser=await chromium.launch({executablePath,headless:true});
  try{
    const origin = `http://127.0.0.1:${address.port}/e2e/fixtures/analytics-interaction.html`;
    const page=await browser.newPage();
    const touchPage=await browser.newPage({hasTouch:true});
    await Promise.all([page.goto(origin), touchPage.goto(origin)]);
    const metric=page.getByRole('slider',{name:'Requests'});await metric.waitFor();
    const touchMetric=touchPage.getByRole('slider',{name:'Requests'});await touchMetric.waitFor();
    // React commits the selected bucket asynchronously and Fluent keeps hidden tooltip portals mounted.
    // Wait for the committed value and scope tooltip checks to visible content before sending the next input.
    const waitForMetricValue = async (targetPage: Page, suffix: string) => {
      await targetPage.waitForFunction((expected: string) => document.querySelector<HTMLElement>('[role="slider"][aria-label="Requests"]')?.getAttribute('aria-valuetext')?.endsWith(expected) ?? false, suffix);
    };
    const visibleTooltips = (targetPage: Page) => targetPage.locator('[role="tooltip"]:visible');
    for(const theme of ['light','dark'])for(const width of [390,1440,2560]){
      await page.setViewportSize({width,height:1000});
      await page.evaluate(theme=>{document.documentElement.dataset.theme=theme;},theme);
      await metric.focus();await page.keyboard.press('Home');
      await waitForMetricValue(page,'Requests: 2');
      assert.match(await metric.getAttribute('aria-valuetext')??'',/UTC · Requests: 2$/);
      await page.keyboard.press('ArrowRight');await waitForMetricValue(page,'Requests: —');assert.match(await metric.getAttribute('aria-valuetext')??'',/Requests: —$/);
      const box=await metric.boundingBox();assert.ok(box);
      await page.mouse.move(box.x+box.width*.7,box.y+box.height*.7);
      await waitForMetricValue(page,'Requests: 10');
      assert.match(await metric.getAttribute('aria-valuetext')??'',/Requests: 10$/);
      const trendTooltip = visibleTooltips(page).filter({ hasText: 'Requests: 10' });
      await trendTooltip.waitFor({ state: 'visible' });
      await page.keyboard.press('Escape');await trendTooltip.waitFor({state:'hidden'});
      assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth),false);
      if(width===2560)assert.ok(await page.locator('.app-main-content').evaluate(el=>el.getBoundingClientRect().width)>2500,'analytics must not retain 2048px shell cap');
      if(width===2560)assert.ok(await page.locator('.usage-page').evaluate(el=>el.getBoundingClientRect().width)>2400,'late operator CSS must not restore the 1360px inner page cap');
    }
    for(const theme of ['light','dark'])for(const width of [390,1440,2560]){
      await touchPage.setViewportSize({width,height:1000});
      await touchPage.evaluate(theme=>{document.documentElement.dataset.theme=theme;},theme);
      await touchMetric.focus();await touchPage.keyboard.press('Home');
      const box=await touchMetric.boundingBox();assert.ok(box);
      await touchPage.touchscreen.tap(box.x+box.width-4,box.y+box.height*.7);
      await waitForMetricValue(touchPage,'Requests: 5');
      assert.match(await touchMetric.getAttribute('aria-valuetext')??'',/Requests: 5$/);
      const trendTooltip = visibleTooltips(touchPage).filter({ hasText: 'Requests: 5' });
      await trendTooltip.waitFor({ state: 'visible' });
      await touchPage.keyboard.press('Escape');await trendTooltip.waitFor({state:'hidden'});
    }
    assert.equal(await page.getByRole('slider').count(),3,'missing series must not invent samples');
    await page.getByRole('slider',{name:'All zero'}).focus();assert.match(await page.getByRole('slider',{name:'All zero'}).getAttribute('aria-valuetext')??'',/: 0$/);
    const assertSettlementDetail = async (targetPage: Page, action: 'pointer' | 'keyboard' | 'touch') => {
      const settlement = targetPage.getByRole('slider', { name: 'Local settlement' });
      const notice = settlement.locator('.metric-label span[tabindex]');
      await targetPage.mouse.move(1, 1);
      await settlement.focus();
      await targetPage.keyboard.press('Escape');
      if (action === 'pointer') await notice.hover();
      if (action === 'keyboard') {
        await targetPage.keyboard.press('Tab');
        await targetPage.waitForFunction(() => document.activeElement?.matches('.analytics-metric[aria-label="Local settlement"] .metric-label span[tabindex]') ?? false);
      }
      if (action === 'touch') await notice.tap();
      await targetPage.waitForFunction(() => {
        const trigger = document.querySelector<HTMLElement>('.analytics-metric[aria-label="Local settlement"] .metric-label span[tabindex]');
        const ids = trigger?.getAttribute('aria-describedby')?.split(/\s+/) ?? [];
        return ids.some(id => document.getElementById(id)?.getAttribute('role') === 'tooltip');
      });
      const describedBy = await notice.getAttribute('aria-describedby');
      assert.ok(describedBy);
      const tooltipId = await targetPage.evaluate(() => {
        const trigger = document.querySelector<HTMLElement>('.analytics-metric[aria-label="Local settlement"] .metric-label span[tabindex]');
        return (trigger?.getAttribute('aria-describedby') ?? '').split(/\s+/).find(id => document.getElementById(id)?.getAttribute('role') === 'tooltip') ?? '';
      });
      assert.ok(tooltipId);
      const tooltip = targetPage.locator(`[role="tooltip"][id=${JSON.stringify(tooltipId)}]`);
      await tooltip.waitFor({ state: 'visible' });
      assert.equal(await visibleTooltips(targetPage).count(), 1, `${action}: detail must not also expose a trend tooltip`);
      const tooltipText = await tooltip.textContent() ?? '';
      assert.match(tooltipText, /保守上限|conservative ceiling|settlement ceiling/i);
      assert.match(tooltipText, /供应商记录|provider records/i);
      assert.equal(await settlement.locator('.metric-value').textContent(), '$0.123456');
      await targetPage.keyboard.press('Escape');
      await tooltip.waitFor({ state: 'hidden' });
    };
    await assertSettlementDetail(page, 'pointer');
    await assertSettlementDetail(page, 'keyboard');
    await assertSettlementDetail(touchPage, 'touch');
  }finally{await browser.close();await server.close();}
});
