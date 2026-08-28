import assert from 'node:assert/strict';
import { World, setWorldConstructor, type IWorldOptions } from '@cucumber/cucumber';
import type { BrowserContext, Page, Response, Route } from 'playwright';
import {
  captureConsoleFailure,
  capturePageFailure,
  captureRequestFailure,
  ExpectedClientErrorNavigationLedger,
  type BrowserFailure,
} from '../expected-navigation-errors.js';
import {
  credentialsAreBoundToDestination, isAllowedLiveDestination, isReadOnlyMethod,
  urlContainsCredential,
} from '../security.js';
import { liveRuntime } from './runtime.js';

export class LiveWorld extends World {
  context?: BrowserContext;
  page?: Page;
  private activeOrigin?: string;
  private readonly browserFailures: BrowserFailure[] = [];
  private readonly expectedClientErrorNavigations = new ExpectedClientErrorNavigationLedger();
  private readonly rejectedMethods: string[] = [];
  private readonly rejectedDestinations: string[] = [];
  private readonly rejectedCredentialURLs: string[] = [];
  private readonly rejectedCredentialHeaders: string[] = [];
  private readonly providerSecretLeaks: string[] = [];
  private readonly responseAuditFailures: string[] = [];
  private readonly auditedResponsePaths = new Set<string>();
  private readonly pendingResponseAudits = new Set<Promise<void>>();

  constructor(options: IWorldOptions) {
    super(options);
  }

  async createBrowserContext(): Promise<void> {
    const configuration = liveRuntime.requireConfiguration();
    const reportableOrigins = new Set([
      configuration.controlURL.origin,
      configuration.gatewayURL.origin,
    ]);
    this.context = await liveRuntime.requireBrowser().newContext({ viewport: { width: 1280, height: 900 } });
    await this.context.route('**/*', (route) => this.guardRoute(route));
    this.page = await this.context.newPage();
    this.page.on('console', (message) => {
      if (message.type() !== 'error') return;
      this.browserFailures.push(captureConsoleFailure(
        message.text(),
        message.location().url,
        this.page?.url() ?? '',
        reportableOrigins,
      ));
    });
    this.page.on('pageerror', (error) => {
      this.browserFailures.push(capturePageFailure(error.name));
    });
    this.page.on('requestfailed', (request) => {
      if (request.url().includes('/internal/v1/request-events')) return;
      if (this.rejectedMethods.includes(request.method().toUpperCase())) return;
      this.browserFailures.push(captureRequestFailure(
        request.method(),
        request.url(),
        request.failure()?.errorText,
        reportableOrigins,
      ));
    });
    this.page.on('response', (response) => {
      const audit = this.auditResponse(response);
      this.pendingResponseAudits.add(audit);
      void audit.finally(() => this.pendingResponseAudits.delete(audit));
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
    const response = await this.navigate(base, path);
    assert.ok(response?.ok(), `live page returned HTTP ${response?.status() ?? 'no response'}`);
  }

  async navigate(base: URL, path: string) {
    this.activeOrigin = base.origin;
    return this.requirePage().goto(new URL(path, base).toString(), { waitUntil: 'domcontentloaded' });
  }

  async navigateExpectingClientError(base: URL, path: string, expectedStatus: number): Promise<Response> {
    const requestedURL = new URL(path, base).href;
    const response = await this.navigate(base, path);
    assert.ok(response, 'expected client-error navigation returned no HTTP response');
    this.expectedClientErrorNavigations.verify(
      requestedURL,
      expectedStatus,
      response.url(),
      response.status(),
    );
    return response;
  }

  async assertProviderSecretAbsent(expectedAPIPaths: readonly string[] = []): Promise<void> {
    await this.awaitResponseAudits();
    const content = await this.requirePage().content();
    assert.ok(!content.includes(liveRuntime.requireConfiguration().providerSecretCanary), 'provider secret canary leaked into the DOM');
    assert.deepEqual(this.providerSecretLeaks, [], 'provider secret canary leaked into an HTTP response');
    for (const path of expectedAPIPaths) {
      assert.ok([...this.auditedResponsePaths].some((audited) => audited.includes(path)), `expected response body audit did not observe ${path}`);
    }
  }

  async assertReadOnlyAndClean(): Promise<void> {
    await this.assertProviderSecretAbsent();
    assert.deepEqual(this.rejectedMethods, [], 'the live application attempted a non-read-only HTTP method');
    assert.deepEqual(this.rejectedDestinations, [], 'the live application attempted a cross-origin or unconfigured request');
    assert.deepEqual(this.rejectedCredentialURLs, [], 'the live application placed a credential in a URL');
    assert.deepEqual(this.rejectedCredentialHeaders, [], 'the live application sent a credential header to the wrong origin');
    assert.deepEqual(this.responseAuditFailures, [], 'the live response canary audit could not inspect an eligible response');
    assert.deepEqual(
      this.expectedClientErrorNavigations.unexpectedFailures(this.browserFailures),
      [],
      'the live browser emitted unexpected console, page, or request failures',
    );
  }

  private async guardRoute(route: Route): Promise<void> {
    const method = route.request().method().toUpperCase();
    if (!isReadOnlyMethod(method)) {
      this.rejectedMethods.push(method);
      await route.abort('blockedbyclient');
      return;
    }
    const configuration = liveRuntime.requireConfiguration();
    const allowedOrigins = new Set([configuration.controlURL.origin, configuration.gatewayURL.origin]);
    const activeOrigin = this.activeOrigin;
    if (!activeOrigin) {
      this.rejectedDestinations.push('unset-active-origin');
      await route.abort('blockedbyclient');
      return;
    }
    if (urlContainsCredential(route.request().url(), [
      configuration.serviceCredential,
      configuration.clientCredential,
      configuration.providerSecretCanary,
    ])) {
      this.rejectedCredentialURLs.push('credential-in-url');
      await route.abort('blockedbyclient');
      return;
    }
    if (!isAllowedLiveDestination(route.request().url(), allowedOrigins, activeOrigin)) {
      this.rejectedDestinations.push(safeOrigin(route.request().url()));
      await route.abort('blockedbyclient');
      return;
    }
    const bindings = [
      { credential: configuration.serviceCredential, origin: configuration.controlURL.origin },
      { credential: configuration.clientCredential, origin: configuration.gatewayURL.origin },
      { credential: configuration.providerSecretCanary, origin: '' },
    ];
    if (!credentialsAreBoundToDestination(route.request().url(), route.request().headers(), bindings)) {
      this.rejectedCredentialHeaders.push(safeOrigin(route.request().url()));
      await route.abort('blockedbyclient');
      return;
    }
    await route.continue();
  }

  private async auditResponse(response: Response): Promise<void> {
    const path = safeResponsePath(response.url());
    try {
      if (!new URL(response.url()).pathname.startsWith('/internal/v1/') && !new URL(response.url()).pathname.startsWith('/self/v1/')) return;
      const contentType = (await response.headerValue('content-type') ?? '').toLowerCase();
      if (!/(?:application\/json|text\/|\+json)/.test(contentType) || contentType.includes('text/event-stream')) return;
      const contentLength = Number(await response.headerValue('content-length') ?? '0');
      if (Number.isFinite(contentLength) && contentLength > 4 * 1024 * 1024) {
        this.responseAuditFailures.push(path);
        return;
      }
      const body = await response.text();
      this.auditedResponsePaths.add(path);
      if (body.includes(liveRuntime.requireConfiguration().providerSecretCanary)) {
        this.providerSecretLeaks.push(path);
      }
    } catch {
      this.responseAuditFailures.push(path);
    }
  }

  private async awaitResponseAudits(): Promise<void> {
    while (this.pendingResponseAudits.size > 0) {
      await Promise.all([...this.pendingResponseAudits]);
    }
  }
}

setWorldConstructor(LiveWorld);

function safeOrigin(value: string): string {
  try { return new URL(value).origin; } catch { return 'invalid-url'; }
}

function safeResponsePath(value: string): string {
  try {
    const url = new URL(value);
    return `${url.origin}${url.pathname}`;
  } catch {
    return 'invalid-url';
  }
}
