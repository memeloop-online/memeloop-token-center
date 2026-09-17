import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

test('analytics background exposes actual indexed buckets through pointer, keyboard and touch', { timeout: 30000 }, async context => {
  const executablePath=process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH??chromium.executablePath();
  if(!existsSync(executablePath)){if(process.env.MTC_REQUIRE_BROWSER==='1')throw new Error('Chromium required');return context.skip('Chromium required');}
  const server=await createServer({root:fileURLToPath(new URL('..',import.meta.url)),configFile:false,logLevel:'silent',server:{host:'127.0.0.1',port:0}});await server.listen();
  const address=server.httpServer!.address();assert.ok(address&&typeof address!=='string');
  const browser=await chromium.launch({executablePath,headless:true});
  try{
    const page=await browser.newPage({hasTouch:true});
    await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/analytics-interaction.html`);
    const metric=page.getByRole('slider',{name:'Requests'});await metric.waitFor();
    // React commits the selected bucket asynchronously and Fluent keeps hidden tooltip portals mounted.
    // Wait for the committed value and scope tooltip checks to visible content before sending the next input.
    const waitForMetricValue = async (suffix: string) => {
      const element = await metric.elementHandle(); assert.ok(element);
      await page.waitForFunction((node, expected) => (node.getAttribute('aria-valuetext') ?? '').endsWith(expected), element, suffix);
    };
    const visibleTooltips = () => page.locator('[role="tooltip"]:visible');
    for(const theme of ['light','dark'])for(const width of [390,1440,2560]){
      await page.setViewportSize({width,height:1000});
      await page.evaluate(theme=>{document.documentElement.dataset.theme=theme;},theme);
      await metric.focus();await page.keyboard.press('Home');
      await waitForMetricValue('Requests: 2');
      assert.match(await metric.getAttribute('aria-valuetext')??'',/UTC · Requests: 2$/);
      await page.keyboard.press('ArrowRight');await waitForMetricValue('Requests: —');assert.match(await metric.getAttribute('aria-valuetext')??'',/Requests: —$/);
      const box=await metric.boundingBox();assert.ok(box);
      await page.mouse.move(box.x+box.width*.7,box.y+box.height*.7);
      await waitForMetricValue('Requests: 10');
      assert.match(await metric.getAttribute('aria-valuetext')??'',/Requests: 10$/);
      await page.touchscreen.tap(box.x+box.width-4,box.y+box.height*.7);
      await waitForMetricValue('Requests: 5');
      assert.match(await metric.getAttribute('aria-valuetext')??'',/Requests: 5$/);
      const trendTooltip = visibleTooltips().filter({ hasText: 'Requests: 5' });
      await trendTooltip.waitFor({ state: 'visible' });
      await page.keyboard.press('Escape');await trendTooltip.waitFor({state:'hidden'});
      assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth),false);
      if(width===2560)assert.ok(await page.locator('.app-main-content').evaluate(el=>el.getBoundingClientRect().width)>2500,'analytics must not retain 2048px shell cap');
      if(width===2560)assert.ok(await page.locator('.usage-page').evaluate(el=>el.getBoundingClientRect().width)>2400,'late operator CSS must not restore the 1360px inner page cap');
    }
    assert.equal(await page.getByRole('slider').count(),3,'missing series must not invent samples');
    await page.getByRole('slider',{name:'All zero'}).focus();assert.match(await page.getByRole('slider',{name:'All zero'}).getAttribute('aria-valuetext')??'',/: 0$/);
    const settlement = page.getByRole('slider', { name: 'Local settlement' });
    const notice = settlement.locator('.metric-label span[tabindex]');
    for (const action of ['pointer', 'keyboard', 'touch']) {
      await page.mouse.move(1, 1);
      await settlement.focus();
      await page.keyboard.press('Escape');
      if (action === 'pointer') await notice.hover();
      if (action === 'keyboard') await page.keyboard.press('Tab');
      if (action === 'touch') await notice.tap();
      const tooltip = visibleTooltips().filter({ hasText: /供应商实际用量或发票请以供应商记录为准|use provider records for actual usage or invoice details/ });
      await tooltip.waitFor({ state: 'visible' });
      assert.equal(await visibleTooltips().count(), 1, `${action}: detail must not also expose a trend tooltip`);
      assert.match(await tooltip.textContent() ?? '', /保守上限|conservative ceiling/);
      assert.equal(await settlement.locator('.metric-value').textContent(), '$0.123456');
      await page.keyboard.press('Escape');
      await tooltip.waitFor({ state: 'hidden' });
    }
  }finally{await browser.close();await server.close();}
});
