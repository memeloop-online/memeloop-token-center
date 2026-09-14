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
    for(const theme of ['light','dark'])for(const width of [390,1440,2560]){
      await page.setViewportSize({width,height:1000});
      await page.evaluate(theme=>{document.documentElement.dataset.theme=theme;},theme);
      await metric.focus();await page.keyboard.press('Home');
      assert.match(await metric.getAttribute('aria-valuetext')??'',/UTC · Requests: 2$/);
      await page.keyboard.press('ArrowRight');assert.match(await metric.getAttribute('aria-valuetext')??'',/Requests: —$/);
      const box=await metric.boundingBox();assert.ok(box);
      await page.mouse.move(box.x+box.width*.7,box.y+box.height*.7);
      assert.match(await metric.getAttribute('aria-valuetext')??'',/Requests: 10$/);
      await page.touchscreen.tap(box.x+box.width-4,box.y+box.height*.7);
      assert.match(await metric.getAttribute('aria-valuetext')??'',/Requests: 5$/);
      await page.getByRole('tooltip').waitFor();
      await page.keyboard.press('Escape');await page.getByRole('tooltip').waitFor({state:'hidden'});
      assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth),false);
      if(width===2560)assert.ok(await page.locator('.app-main-content').evaluate(el=>el.getBoundingClientRect().width)>2500,'analytics must not retain 2048px shell cap');
      if(width===2560)assert.ok(await page.locator('.usage-page').evaluate(el=>el.getBoundingClientRect().width)>2400,'late operator CSS must not restore the 1360px inner page cap');
    }
    assert.equal(await page.getByRole('slider').count(),2,'missing series must not invent samples');
    await page.getByRole('slider',{name:'All zero'}).focus();assert.match(await page.getByRole('slider',{name:'All zero'}).getAttribute('aria-valuetext')??'',/: 0$/);
  }finally{await browser.close();await server.close();}
});
