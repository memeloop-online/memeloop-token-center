import assert from 'node:assert/strict';
import { spawn, type ChildProcess } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium, type Browser } from 'playwright';

const bootstrapToken = process.env.MTC_E2E_SERVICE_TOKEN
  ?? `browser-e2e-bootstrap-not-a-real-token-${randomUUID()}`;
const mockPort = Number(process.env.MTC_E2E_MOCK_PORT ?? 41740);
export const baseURL = new URL(process.env.MTC_E2E_BASE_URL ?? 'http://127.0.0.1:41739');
export const tenant = 'browser-e2e-tenant';
export const model = 'browser-e2e-model';

export interface GenerationMockCounts {
  image: number;
  video: number;
}

export async function generationMockCounts(): Promise<GenerationMockCounts> {
  const response = await fetch(`http://127.0.0.1:${mockPort}/__e2e/generation-counts`, {
    signal: AbortSignal.timeout(2_000),
  });
  assert.equal(response.status, 200, 'generation mock count endpoint must be available');
  return await response.json() as GenerationMockCounts;
}

export interface SeedState {
  clientCredential: string;
  clientKeyId: string;
  otherClientCredential: string;
  otherClientKeyId: string;
  globalServiceCredential: string;
  serviceCredential: string;
  upstreamId: string;
  upstreamName: string;
  otherUpstreamId: string;
  otherUpstreamName: string;
  routeId: string;
}

interface JsonRequest {
  credential?: string;
  method?: 'GET' | 'POST' | 'PUT' | 'PATCH' | 'DELETE';
  body?: unknown;
  headers?: Record<string, string>;
}

class E2ERuntime {
  browser?: Browser;
  seed?: SeedState;
  private server?: ChildProcess;
  private serverExit?: Promise<{ code: number | null; signal: NodeJS.Signals | null }>;

  async start(): Promise<void> {
    assert.equal(this.server, undefined, 'the e2e runtime must start only once');
    const serverPath = fileURLToPath(new URL('../server.mjs', import.meta.url));
    const webRoot = resolve(dirname(serverPath), '..');
    const server = spawn(process.execPath, [serverPath], {
      cwd: webRoot,
      env: { ...process.env, MTC_E2E_SERVICE_TOKEN: bootstrapToken },
      stdio: ['inherit', 'inherit', 'inherit', 'ipc'],
    });
    this.server = server;
    this.serverExit = new Promise((resolveExit) => {
      server.once('exit', (code, signal) => resolveExit({ code, signal }));
    });
    await this.waitUntilMockOwned();
    await this.waitUntilReady();
    this.seed = await seedThroughHttp();
    this.browser = await chromium.launch({ headless: true, executablePath: chromium.executablePath() });
  }

  async stop(): Promise<void> {
    const browser = this.browser;
    this.browser = undefined;
    if (browser) await browser.close();

    const server = this.server;
    const serverExit = this.serverExit;
    this.server = undefined;
    this.serverExit = undefined;
    if (!server || !serverExit) return;
    if (server.exitCode === null && server.signalCode === null) {
      server.kill('SIGTERM');
      const exited = await Promise.race([
        serverExit.then(() => true),
        delay(15_000).then(() => false),
      ]);
      if (!exited && server.exitCode === null && server.signalCode === null) {
        server.kill('SIGKILL');
        await Promise.race([serverExit, delay(5_000)]);
      }
    }
    await this.waitUntilPortsReleased();
  }

  requireBrowser(): Browser {
    assert.ok(this.browser, 'Cucumber browser runtime is not initialized');
    return this.browser;
  }

  requireSeed(): SeedState {
    assert.ok(this.seed, 'Cucumber HTTP fixtures are not initialized');
    return this.seed;
  }

  private async waitUntilReady(): Promise<void> {
    const deadline = Date.now() + 300_000;
    let lastReason = 'service did not respond';
    while (Date.now() < deadline) {
      const exit = await Promise.race([
        this.serverExit!.then((value) => value),
        delay(0).then(() => undefined),
      ]);
      if (exit) throw new Error(`browser e2e server exited before readiness (${exit.code ?? exit.signal})`);
      try {
        const response = await fetch(new URL('/healthz', baseURL), { signal: AbortSignal.timeout(2_000) });
        if (response.ok) return;
        lastReason = `health endpoint returned ${response.status}`;
      } catch (reason) {
        lastReason = reason instanceof Error ? reason.message : String(reason);
      }
      await delay(250);
    }
    throw new Error(`browser e2e service was not ready within 300 seconds: ${lastReason}`);
  }

  private async waitUntilMockOwned(): Promise<void> {
    const server = this.server!;
    const exit = this.serverExit!;
    await Promise.race([
      new Promise<void>((resolveReady) => {
        const onMessage = (message: unknown) => {
          if (!message || typeof message !== 'object' || !('type' in message)
            || message.type !== 'mock-listening') return;
          server.off('message', onMessage);
          resolveReady();
        };
        server.on('message', onMessage);
      }),
      exit.then(({ code, signal }) => {
        throw new Error(`browser e2e server exited before owning its mock port (${code ?? signal})`);
      }),
      delay(10_000).then(() => {
        throw new Error('browser e2e server did not acquire its mock port within 10 seconds');
      }),
    ]);
  }

  private async waitUntilPortsReleased(): Promise<void> {
    const mockURL = new URL(`http://127.0.0.1:${mockPort}/`);
    const deadline = Date.now() + 5_000;
    while (Date.now() < deadline) {
      const [applicationReachable, mockReachable] = await Promise.all([
        endpointIsReachable(new URL('/healthz', baseURL)),
        endpointIsReachable(mockURL),
      ]);
      if (!applicationReachable && !mockReachable) return;
      await delay(100);
    }
    throw new Error('browser e2e service ports remained reachable after shutdown');
  }
}

export const runtime = new E2ERuntime();

export async function requestJson<T>(path: string, options: JsonRequest = {}): Promise<T> {
  const headers = new Headers(options.headers);
  if (options.credential) headers.set('Authorization', `Bearer ${options.credential}`);
  if (options.body !== undefined) headers.set('Content-Type', 'application/json');
  const response = await fetch(new URL(path, baseURL), {
    method: options.method ?? 'GET',
    headers,
    body: options.body === undefined ? undefined : JSON.stringify(options.body),
    signal: AbortSignal.timeout(30_000),
  });
  const text = await response.text();
  assert.ok(response.ok, `${options.method ?? 'GET'} ${path}: ${response.status} ${text}`);
  return (text ? JSON.parse(text) : undefined) as T;
}

async function seedThroughHttp(): Promise<SeedState> {
  const upstream = await requestJson<{ id: string }>('/internal/v1/upstreams', {
    method: 'POST', credential: bootstrapToken,
    body: {
      tenant_external_id: tenant,
      name: 'Browser mock upstream',
      driver: 'http-json',
      config: { base_url: `http://127.0.0.1:${mockPort}` },
      credential: { type: 'api_key', value: 'browser-mock-upstream-not-a-secret' },
    },
  });
  await eventually(async () => {
    const catalog = await requestJson<{ status: string; models: Array<{ id: string }> }>(
      `/internal/v1/upstreams/${upstream.id}/models/sync?tenant_external_id=${encodeURIComponent(tenant)}`,
      { method: 'POST', credential: bootstrapToken },
    );
    assert.equal(catalog.status, 'ready');
    assert.ok(catalog.models.length > 0);
  }, 30_000, 'mock upstream model catalog did not become ready');
  const otherUpstream = await requestJson<{ id: string }>('/internal/v1/upstreams', {
    method: 'POST', credential: bootstrapToken,
    body: {
      tenant_external_id: tenant,
      name: 'Browser secondary upstream',
      driver: 'http-json',
      config: { base_url: `http://127.0.0.1:${mockPort}` },
      credential: { type: 'api_key', value: 'browser-secondary-upstream-not-a-secret' },
    },
  });
  await eventually(async () => {
    const catalog = await requestJson<{ status: string; models: Array<{ id: string }> }>(
      `/internal/v1/upstreams/${otherUpstream.id}/models/sync?tenant_external_id=${encodeURIComponent(tenant)}`,
      { method: 'POST', credential: bootstrapToken },
    );
    assert.equal(catalog.status, 'ready');
    assert.ok(catalog.models.length > 0);
  }, 30_000, 'secondary mock upstream model catalog did not become ready');
  const route = await requestJson<{ id: string }>('/internal/v1/model-routes', {
    method: 'POST', credential: bootstrapToken,
    body: {
      tenant_external_id: tenant,
      public_model: model,
      upstream_account_id: upstream.id,
      upstream_model: 'mock-provider-model',
      protocol: 'openai',
      priority: 0,
    },
  });
  await requestJson(`/internal/v1/prices/USD/${model}`, {
    method: 'POST', credential: bootstrapToken,
    body: { input_per_million: '1', output_per_million: '2' },
  });
  for (const price of [
    { model: 'browser-image-model', billing_unit: 'image', price_per_unit: '0.4' },
    { model: 'browser-video-model', billing_unit: 'second', price_per_unit: '0.1' },
    { model: 'browser-workflow-model', billing_unit: 'job', price_per_unit: '0.25' },
    { model: 'browser-megapixel-model', billing_unit: 'megapixel', price_per_unit: '0.02' },
  ]) {
    await requestJson(`/internal/v1/generation-prices/USD/${price.model}`, {
      method: 'POST', credential: bootstrapToken,
      body: { billing_unit: price.billing_unit, price_per_unit: price.price_per_unit },
    });
  }
  const client = await requestJson<{ key: string; key_id: string }>('/internal/v1/keys', {
    method: 'POST', credential: bootstrapToken,
    body: {
      tenant_external_id: tenant,
      principal_external_id: 'browser-e2e-user',
      alias: 'Browser E2E credential',
      currency: 'USD',
      initial_balance: '1000',
      policy: {
        requests_per_minute: 1000,
        tokens_per_minute: 200000000,
        max_concurrency: 4,
        daily_budget: null,
        weekly_budget: null,
        lifetime_budget: '1000',
      },
      route_ids: [route.id],
      route_group_ids: [],
    },
  });
  const otherClient = await requestJson<{ key: string; key_id: string }>('/internal/v1/keys', {
    method: 'POST', credential: bootstrapToken,
    body: {
      tenant_external_id: tenant,
      principal_external_id: 'browser-e2e-other-user',
      alias: 'Browser E2E other credential',
      currency: 'USD',
      initial_balance: '1',
      policy: {
        requests_per_minute: 1,
        tokens_per_minute: 100,
        max_concurrency: 1,
        daily_budget: null,
        weekly_budget: null,
        lifetime_budget: '1',
      },
      route_ids: [route.id],
      route_group_ids: [],
    },
  });
  const service = await requestJson<{ token: string }>('/internal/v1/service-tokens', {
    method: 'POST', credential: bootstrapToken,
    body: {
      name: 'Browser E2E tenant operator',
      tenant_external_id: tenant,
      scopes: [
        'requests:read', 'providers:read', 'providers:write', 'plugins:read', 'plugins:write', 'schemas:read',
        'keys:read', 'keys:write', 'routes:read', 'routes:write', 'prices:read', 'prices:write', 'oauth:write',
      ],
    },
  });
  const globalService = await requestJson<{ token: string }>('/internal/v1/service-tokens', {
    method: 'POST', credential: bootstrapToken,
    body: {
      name: 'Browser E2E global operator',
      scopes: [
        'requests:read', 'providers:read', 'providers:write', 'plugins:read', 'plugins:write', 'schemas:read',
        'keys:read', 'keys:write', 'routes:read', 'routes:write', 'prices:read', 'prices:write', 'oauth:write',
      ],
    },
  });

  await requestJson('/v1/chat/completions', {
    method: 'POST', credential: client.key,
    body: {
      model,
      messages: [{ role: 'user', content: 'Create one observable browser test request.' }],
      max_tokens: 32,
    },
  });
  // This is browser workflow coverage, not SQLite write-concurrency coverage.
  // Sequential traffic prevents a temporary SQLite finalizer from outliving a slow shared runner.
  for (let requestNumber = 2; requestNumber <= 50; requestNumber += 1) {
    await requestJson('/v1/chat/completions', {
      method: 'POST', credential: client.key,
      body: {
        model,
        messages: [{ role: 'user', content: `Create observable browser test request ${requestNumber}.` }],
        max_tokens: 32,
      },
    });
  }
  const failed = await fetch(new URL('/v1/chat/completions', baseURL), {
    method: 'POST',
    headers: { Authorization: `Bearer ${client.key}`, 'Content-Type': 'application/json' },
    body: JSON.stringify({
      model,
      messages: [{ role: 'user', content: 'force observable error' }],
      max_tokens: 32,
    }),
    signal: AbortSignal.timeout(30_000),
  });
  assert.equal(failed.status, 429);

  await eventually(async () => {
    const stats = await requestJson<{
      summary: { total_requests: number; successful_requests: number; failed_requests: number };
    }>('/self/v1/stats', { credential: client.key });
    const counts = [
      stats.summary.total_requests,
      stats.summary.successful_requests,
      stats.summary.failed_requests,
    ];
    if (counts[0] === 51 && (counts[1] !== 50 || counts[2] !== 1)) {
      const requests = await requestJson<Array<{
        request_id: string;
        status_code: number | null;
        error_code: string | null;
        input_tokens: number;
        output_tokens: number;
      }>>('/self/v1/requests?limit=100', { credential: client.key });
      const failures = requests
        .filter((request) => request.status_code === null || request.status_code >= 400)
        .map(({ request_id: _requestId, ...safe }) => safe);
      assert.deepEqual(counts, [51, 50, 1], `unexpected terminal request records: ${JSON.stringify(failures)}`);
    }
    assert.deepEqual(counts, [51, 50, 1]);
  }, 30_000, 'fixture statistics did not settle');

  return {
    clientCredential: client.key,
    clientKeyId: client.key_id,
    otherClientCredential: otherClient.key,
    globalServiceCredential: globalService.token,
    otherClientKeyId: otherClient.key_id,
    serviceCredential: service.token,
    upstreamId: upstream.id,
    upstreamName: 'Browser mock upstream',
    otherUpstreamId: otherUpstream.id,
    otherUpstreamName: 'Browser secondary upstream',
    routeId: route.id,
  };
}

export async function eventually(
  assertion: () => void | Promise<void>,
  timeout = 10_000,
  message = 'condition did not become true',
): Promise<void> {
  const deadline = Date.now() + timeout;
  let lastError: unknown;
  do {
    try {
      await assertion();
      return;
    } catch (reason) {
      lastError = reason;
      await delay(100);
    }
  } while (Date.now() < deadline);
  const detail = lastError instanceof Error ? lastError.message : String(lastError);
  throw new Error(`${message}: ${detail}`);
}

function delay(milliseconds: number): Promise<void> {
  return new Promise((resolveDelay) => setTimeout(resolveDelay, milliseconds));
}

async function endpointIsReachable(url: URL): Promise<boolean> {
  try {
    await fetch(url, { signal: AbortSignal.timeout(250) });
    return true;
  } catch {
    return false;
  }
}
