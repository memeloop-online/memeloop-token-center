import assert from 'node:assert/strict';
import { World, setWorldConstructor, type IWorldOptions } from '@cucumber/cucumber';
import type { BrowserContext, Page } from 'playwright';
import { baseURL, runtime } from './runtime.js';
import { isExpectedResourceAbort } from './request-failures.js';

export class DogfoodWorld extends World {
  context?: BrowserContext;
  page?: Page;
  readonly consoleErrors: string[] = [];
  readonly failedRequests: string[] = [];
  readonly serverErrorPaths: string[] = [];
  readonly clientErrorPaths: string[] = [];

  constructor(options: IWorldOptions) {
    super(options);
  }

  async createBrowserContext(): Promise<void> {
    this.context = await runtime.requireBrowser().newContext({
      baseURL: baseURL.toString(),
      viewport: { width: 1280, height: 900 },
    });
    this.page = await this.context.newPage();
    this.page.on('console', (message) => {
      if (message.type() === 'error') this.consoleErrors.push(message.text());
    });
    this.page.on('pageerror', (error) => this.consoleErrors.push(error.message));
    this.page.on('requestfailed', (request) => {
      const failure = request.failure()?.errorText ?? 'unknown';
      // React intentionally aborts the long-lived SSE tail during a tab or tenant change.
      if (request.url().includes('/internal/v1/request-events') && failure.includes('ERR_ABORTED')) return;
      // Catalog searches and monitoring snapshots cancel superseded reads on
      // query, tenant or page changes; other failures must remain observable.
      if (isExpectedResourceAbort(request.method(), request.url(), failure)) return;
      this.failedRequests.push(`${request.method()} ${request.url()}: ${failure}`);
    });
    this.page.on('response', (response) => {
      if (response.status() >= 400 && response.status() < 500) {
        // Diagnostic only: preserve existing expected-auth-error assertions.
        // No origin, query, credentials, request body or response body is logged.
        const path = new URL(response.url()).pathname;
        const category = ['/internal/v1/plugin-runtime', '/internal/v1/plugin-runtime/history'].includes(path)
          ? path : path.startsWith('/internal/') ? '/internal/[other]' : '/[other]';
        this.clientErrorPaths.push(`${response.status()} ${category}`);
      }
      if (response.status() < 500) return;
      // Keep CI diagnostics safe: a pathname carries neither query values,
      // credentials, nor response content.
      this.serverErrorPaths.push(new URL(response.url()).pathname);
    });
  }

  requirePage(): Page {
    assert.ok(this.page, 'scenario page is not initialized');
    return this.page;
  }

  async open(
    path: string,
    options: { theme: 'dark' | 'light'; locale: 'zh-CN' | 'en'; viewport?: { width: number; height: number } },
  ): Promise<void> {
    const page = this.requirePage();
    await page.addInitScript(({ theme, locale }) => {
      localStorage.setItem('mtc-theme', theme);
      localStorage.setItem('mtc-locale', locale);
      sessionStorage.clear();
    }, options);
    if (options.viewport) await page.setViewportSize(options.viewport);
    await page.goto(path);
  }

  assertNoBrowserFailures(): void {
    assert.deepEqual(this.serverErrorPaths, [], 'browser HTTP 5xx responses were observed');
    assert.deepEqual(this.consoleErrors, [], `browser console or page errors were observed; HTTP 4xx paths: ${JSON.stringify(this.clientErrorPaths)}`);
    assert.deepEqual(this.failedRequests, [], 'browser requests failed');
  }
}

setWorldConstructor(DogfoodWorld);
