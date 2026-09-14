import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';
import { createServer } from 'vite';

declare global { interface Window { timeFixture: { epoch: number; reads: () => number; chartLabels: () => string[] | undefined; todayQuery: () => string | undefined } } }
test('UTC bucket epochs display in the browser zone while local day queries keep their real boundaries', { timeout: 30000 }, async context => {
 const executablePath=process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH??chromium.executablePath();
 if(!existsSync(executablePath)){if(process.env.MTC_REQUIRE_BROWSER==='1')throw new Error('Chromium required');return context.skip('Chromium required');}
 const server=await createServer({root:fileURLToPath(new URL('..',import.meta.url)),configFile:false,logLevel:'silent',server:{host:'127.0.0.1',port:0}});await server.listen();
 const address=server.httpServer!.address();assert.ok(address&&typeof address!=='string');
 const browser=await chromium.launch({executablePath,headless:true});
 try{
  for(const timezoneId of ['Asia/Shanghai','UTC']){
   const page=await browser.newPage({timezoneId,locale:'zh-CN'});
   await page.addInitScript(()=>localStorage.setItem('mtc-locale','zh-CN'));
   await page.goto(`http://127.0.0.1:${address.port}/e2e/fixtures/analytics-timezone.html`);
   await page.waitForFunction(()=>Boolean(window.timeFixture?.chartLabels()?.length));
   const hour=timezoneId==='Asia/Shanghai'?'22:00':'14:00';
   assert.match((await page.evaluate(()=>window.timeFixture.chartLabels()))![0],new RegExp(hour));
   // Use stable metric position: translation changes must not hide time semantics.
   const first=page.locator('.usage-metrics [role="slider"]').first();await first.focus();
   assert.match(await first.getAttribute('aria-valuetext')??'',new RegExp(hour));
   assert.match(await first.getAttribute('aria-valuetext')??'',new RegExp(timezoneId));
   const reads=await page.evaluate(()=>window.timeFixture.reads());
   const card=page.locator('.usage-chart-card').first();
   const chartTab=card.getByRole('tab',{name:'图表',exact:true});
   const dataTab=card.getByRole('tab',{name:'数据',exact:true});
   await chartTab.focus();await chartTab.press('ArrowRight');await page.keyboard.press('Enter');
   assert.equal(await dataTab.getAttribute('aria-selected'),'true');
   assert.equal(await card.locator('.usage-echart').isVisible(),false);
   assert.match(await page.locator('.usage-chart-table tbody tr td').first().innerText(),new RegExp(hour));
   await chartTab.click();
   assert.equal(await card.locator('.usage-echart').isVisible(),true);
   assert.equal(await page.evaluate(()=>window.timeFixture.reads()),reads,'switching views is presentation only');
   assert.equal(await card.locator('details, summary').count(),0);
   await page.locator('#usage-tab-heatmap').click();
   const heatmap=page.locator('.usage-heatmap-panel');
   await heatmap.getByRole('tab',{name:'数据',exact:true}).click();
   await heatmap.locator('.usage-heatmap-table tbody button').first().click();
   assert.match(await heatmap.locator('.usage-heatmap-selection').innerText(),/已选择/);
   assert.equal(await page.evaluate(()=>window.timeFixture.reads()),reads,'heatmap view and row selection also reuse the loaded projection');
   const range=await page.evaluate(()=>{const params=new URLSearchParams(window.timeFixture.todayQuery());const start=Number(params.get('from_created_at'));const local=new Date(start);return {hour:local.getHours(),minute:local.getMinutes(),epoch:window.timeFixture.epoch};});
   assert.equal(range.hour,0);assert.equal(range.minute,0);assert.equal(range.epoch,Date.UTC(2026,8,14,14));
   await page.close();
  }
 }finally{await browser.close();await server.close();}
});
