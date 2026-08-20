import assert from 'node:assert/strict';
import { World, setWorldConstructor, type IWorldOptions } from '@cucumber/cucumber';
import type { BrowserContext, Page, Route } from 'playwright';
import { isReadOnlyMethod } from '../security.js';
import { liveRuntime } from './runtime.js';

export class LiveWorld extends World {
  context?: BrowserContext;
  page?: Page;
  private browserErrorCount = 0;
  private failedRequestCount = 0;
  private readonly rejectedMethods: string[] = [];

  constructor(options: IWorldOptions) {
    super(options);
  }

  async createBrowserContext(): Promise<void> {
    this.context = await liveRuntime.requireBrowser().newContext({ viewport: { width: 1280, height: 900 } });
    await this.context.route('**/*', (route) => this.guardRoute(route));
    this.page = await this.context.newPage();
    this.page.on('console', (message) => { if (message.type() === 'error') this.browserErrorCount += 1; });
    this.page.on('pageerror', () => { this.browserErrorCount += 1; });
    this.page.on('requestfailed', (request) => {
      if (request.url().includes('/internal/v1/request-events')) return;
      if (this.rejectedMethods.includes(request.method().toUpperCase())) return;
      this.failedRequestCount += 1;
    });
  }

  requirePage(): Page {
    assert.ok(this.page, 'live scenario page is not initialized');
    return this.page;
  }

  async open(base: URL, path: string, locale: 'zh-CN' | 'en', theme: 'dark' | 'light'): Promise<void> {
    const page = this.requirePage();
    await page.addInitScript(({ selectedLocale, selectedTheme }) => {
      localStorage.setItem('mtc-locale', selectedLocale);
      localStorage.setItem('mtc-theme', selectedTheme);
      sessionStorage.clear();
    }, { selectedLocale: locale, selectedTheme: theme });
    const response = await page.goto(new URL(path, base).toString(), { waitUntil: 'domcontentloaded' });
    assert.ok(response?.ok(), `live page returned HTTP ${response?.status() ?? 'no response'}`);
  }

  assertReadOnlyAndClean(): void {
    assert.deepEqual(this.rejectedMethods, [], 'the live application attempted a non-read-only HTTP method');
    assert.equal(this.browserErrorCount, 0, 'the live browser emitted console or page errors');
    assert.equal(this.failedRequestCount, 0, 'the live browser observed failed requests');
  }

  private async guardRoute(route: Route): Promise<void> {
    const method = route.request().method().toUpperCase();
    if (!isReadOnlyMethod(method)) {
      this.rejectedMethods.push(method);
      await route.abort('blockedbyclient');
      return;
    }
    await route.continue();
  }
}

setWorldConstructor(LiveWorld);
