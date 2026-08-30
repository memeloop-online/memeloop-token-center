import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import type { Locator, Page } from 'playwright';
import { baseURL, eventually, model, runtime, tenant } from '../support/runtime.js';
import type { DogfoodWorld } from '../support/world.js';

export interface RealtimeReconnectObservation {
  connectionUrls: string[];
  disconnectedForMs: number;
  finalCursorId: string;
  finalRowCount: number;
}
export interface StrictUsageObservation {
  requestUrls: string[];
}
export interface MultimodalObservation {
  blockerCredential: string;
  clientCredential: string;
  clientKeyId: string;
  imageModel: string;
  videoModel: string;
  generationResponses: string[];
}
export interface CredentialGroupObservation {
  routing: unknown;
  models: unknown;
}

export const realtimeReconnectObservations = new WeakMap<DogfoodWorld, RealtimeReconnectObservation>();
export const strictUsageObservations = new WeakMap<DogfoodWorld, StrictUsageObservation>();
export const multimodalObservations = new WeakMap<DogfoodWorld, MultimodalObservation>();
export const credentialGroupObservations = new WeakMap<DogfoodWorld, CredentialGroupObservation>();
export const uuidPattern = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;
export const groupedModel = 'browser-group-routed-model';

export async function connectOperator(world: DogfoodWorld, theme: 'dark' | 'light', credential?: string): Promise<void> {
  const page = world.requirePage();
  const seed = runtime.requireSeed();
  await world.open('/operator', { theme, locale: 'zh-CN' });
  await page.locator('input[type="password"]').fill(credential ?? seed.serviceCredential);
  await page.getByRole('button', { name: '连接', exact: true }).click();
  const tenantPicker = page.locator('.tenant-picker select');
  await assertContains(tenantPicker, tenant);
  if (await tenantPicker.inputValue() !== tenant) {
    const scopedReload = page.waitForResponse((response) => {
      const url = new URL(response.url());
      return response.request().method() === 'GET'
        && url.pathname === '/internal/v1/upstreams'
        && url.searchParams.get('tenant_external_id') === tenant;
    });
    await tenantPicker.selectOption(tenant);
    assert.equal((await scopedReload).status(), 200);
  }
  await assertValue(tenantPicker, tenant);
  await assertNoCount(page.locator('.notice.error'));
}

export function requireMultimodalObservation(world: DogfoodWorld): MultimodalObservation {
  const observation = multimodalObservations.get(world);
  assert.ok(observation, 'the browser multimodal fixture was not created');
  return observation;
}

export function generationTableFor(page: Page): Locator {
  return page.locator('.self-generations');
}

export function operatorTrafficPanel(page: Page): Locator {
  return page.locator('article.panel').filter({ has: page.locator('.traffic-filters') });
}

export async function submitPortalGeneration(
  page: Page,
  kind: 'image' | 'video',
  generationModel: string,
  prompt: string,
  duration = '5',
  parameters: Record<string, string> = {},
  expectsDuration = kind === 'video',
): Promise<unknown> {
  const panel = page.locator('.generation-create');
  const kindSelect = panel.getByLabel('生成类型');
  await kindSelect.selectOption(kind);
  await assertValue(kindSelect, kind);
  const modelInput = panel.getByLabel('模型');
  const promptInput = panel.getByLabel('提示词');
  await modelInput.selectOption(generationModel);
  await assertValue(modelInput, generationModel);
  await promptInput.fill(prompt);
  await assertValue(promptInput, prompt);
  const durationInput = panel.getByLabel('时长（秒）');
  if (expectsDuration) {
    await assertVisible(durationInput);
    await durationInput.fill(duration);
    await assertValue(durationInput, duration);
  } else {
    await assertNoCount(durationInput);
  }
  if (Object.keys(parameters).length) await assertVisible(panel.getByRole('heading', { name: '任务参数', exact: true }));
  for (const [label, value] of Object.entries(parameters)) {
    const schemaProperty = label === '宽度' ? 'width' : label === '高度' ? 'height' : '';
    const field = schemaProperty ? panel.locator('#root_' + schemaProperty) : panel.getByLabel(label, { exact: true });
    await field.fill(value);
  }
  const endpoint = kind === 'video' ? '/v1/videos/generations' : '/v1/images/generations';
  const submit = panel.getByRole('button', { name: '开始生成', exact: true });
  await eventually(async () => {
    assert.equal(await submit.isEnabled(), true, `${kind} generation submit must be enabled`);
  });
  const requestPromise = page.waitForRequest(
    (request) => request.url().endsWith(endpoint) && request.method() === 'POST',
    { timeout: 10_000 },
  );
  const responsePromise = page.waitForResponse((response) => response.url().endsWith(endpoint) && response.request().method() === 'POST');
  await submit.click();
  const request = await requestPromise;
  assert.equal(new URL(request.url()).pathname, endpoint);
  const response = await responsePromise;
  assert.equal(response.status(), 202);
  await assertContains(panel.getByRole('status'), '任务已创建');
  return request.postDataJSON();
}

export async function waitForGenerationStatus(page: Page, generationModel: string, status: string): Promise<Locator> {
  const row = generationTableFor(page).locator('tbody tr').filter({ hasText: generationModel }).first();
  await eventually(async () => assert.ok(((await row.textContent()) ?? '').includes(status)), 20_000,
    `${generationModel} did not reach ${status} through portal polling`);
  return row;
}

export async function assertGenerationDownload(page: Page, generationModel: string, filename: string, expectedBody: string): Promise<void> {
  const row = generationTableFor(page).locator('tbody tr').filter({ hasText: generationModel }).first();
  await row.getByRole('button', { name: `查看 ${generationModel} 生成任务`, exact: true }).click();
  const drawer = page.getByRole('dialog');
  const downloadPromise = page.waitForEvent('download');
  const assetRequestPromise = page.waitForRequest((request) => new URL(request.url()).pathname.includes('/self/v1/generations/') && new URL(request.url()).pathname.includes('/assets/'));
  await drawer.getByRole('button', { name: '下载文件', exact: true }).click();
  const assetRequest = await assetRequestPromise;
  const assetResponse = await assetRequest.response();
  assert.ok(assetResponse, `generation asset request did not receive a response: ${assetRequest.failure()?.errorText ?? 'unknown failure'}`);
  assert.equal(assetResponse.status(), 200, `generation asset request failed: ${await assetResponse.text()}`);
  const download = await downloadPromise;
  assert.equal(download.suggestedFilename(), filename);
  const path = await download.path();
  assert.ok(path, 'Playwright did not persist the generated asset download');
  assert.equal((await readFile(path)).toString('utf8'), expectedBody);
  await drawer.getByRole('button', { name: '关闭', exact: true }).click();
}

export function requestEventFixture(eventId: string, requestId: string, eventAt: number, eventModel: string) {
  return {
    event_id: eventId,
    request_id: requestId,
    event_at: eventAt,
    event_kind: 'finished' as const,
    key_id: '019f0000-0000-7000-a000-000000000001',
    protocol: 'openai',
    model: eventModel,
    status_code: 200,
    duration_ms: 42,
    input_tokens: 5,
    output_tokens: 7,
    cost: '0.000019',
    error_code: null,
  };
}

export function sseRequestEvent(event: ReturnType<typeof requestEventFixture>): string {
  return `id: ${event.event_id}\nevent: request.${event.event_kind}\ndata: ${JSON.stringify(event)}\n\n`;
}

export function metric(page: Page, label: string): Locator {
  return page.locator('.metric').filter({ hasText: label }).locator('strong');
}

export async function assertVisible(locator: Locator): Promise<void> {
  await locator.first().waitFor({ state: 'visible', timeout: 10_000 });
}

export async function assertContains(locator: Locator, expected: string): Promise<void> {
  await eventually(async () => {
    const text = (await locator.first().textContent()) ?? '';
    assert.ok(text.includes(expected), `expected ${JSON.stringify(text)} to contain ${JSON.stringify(expected)}`);
  });
}

export async function assertNotContains(locator: Locator, expected: string): Promise<void> {
  await eventually(async () => {
    const text = (await locator.first().textContent()) ?? '';
    assert.ok(!text.includes(expected), `expected ${JSON.stringify(text)} not to contain ${JSON.stringify(expected)}`);
  });
}

export async function assertExactText(locator: Locator, expected: string): Promise<void> {
  await eventually(async () => {
    assert.equal(((await locator.first().textContent()) ?? '').trim(), expected);
  });
}

export async function assertCount(locator: Locator, expected: number): Promise<void> {
  await eventually(async () => assert.equal(await locator.count(), expected));
}

export async function assertNoCount(locator: Locator): Promise<void> {
  await eventually(async () => {
    const count = await locator.count();
    assert.equal(count, 0, `expected no matches, found: ${JSON.stringify(await locator.allTextContents())}`);
  });
}

export async function assertValue(locator: Locator, expected: string): Promise<void> {
  await eventually(async () => assert.equal(await locator.inputValue(), expected));
}

export async function assertAttribute(locator: Locator, name: string, expected: string): Promise<void> {
  await eventually(async () => assert.equal(await locator.first().getAttribute(name), expected));
}

export async function applyUsageFilter(
  page: Page,
  change: () => Promise<void>,
  parameter: string,
  expectedValue: string,
  expectedRequests: number,
): Promise<void> {
  await openUsageFilters(page);
  await change();
  const responsePromise = page.waitForResponse((response) => {
    if (!response.url().includes('/internal/v1/usage-analysis?')) return false;
    return new URL(response.url()).searchParams.get(parameter) === expectedValue;
  });
  await page.locator('.usage-controls').getByRole('button', { name: '应用', exact: true }).click();
  const response = await responsePromise;
  assert.equal(response.status(), 200);
  await page.getByRole('tab', { name: '总览', exact: true }).click();
  await assertExactText(metric(page, '请求数'), String(expectedRequests));
}

export async function clearUsageFilters(page: Page, expectedRequests = 51): Promise<void> {
  await openUsageFilters(page);
  const responsePromise = page.waitForResponse((response) => {
    if (!response.url().includes('/internal/v1/usage-analysis?')) return false;
    const query = new URL(response.url()).searchParams;
    return !query.has('model') && !query.has('key_id') && !query.has('upstream_account_id') && !query.has('status');
  });
  await page.locator('.usage-controls').getByRole('button', { name: '清除筛选', exact: true }).click();
  const response = await responsePromise;
  assert.equal(response.status(), 200);
  await assertExactText(metric(page, '请求数'), String(expectedRequests));
}

export function usageDimension(page: Page, heading: string) {
  return page.locator('.usage-dimension').filter({ hasText: heading }).first();
}

export function requireStrictUsageObservation(world: DogfoodWorld) {
  const observation = strictUsageObservations.get(world);
  assert.ok(observation, 'strict usage fixture observation is missing');
  return observation;
}

export async function nextStrictUsageUrl(observation: StrictUsageObservation, previousCount: number) {
  await eventually(
    () => assert.ok(observation.requestUrls.length > previousCount),
    10_000,
    'strict usage fixture did not receive the dimension drilldown request',
  );
  return new URL(observation.requestUrls.at(-1)!);
}

export async function clearStrictUsageFilters(world: DogfoodWorld, expectedRequests: number) {
  const page = world.requirePage();
  await openUsageFilters(page);
  const observation = requireStrictUsageObservation(world);
  const previousCount = observation.requestUrls.length;
  await page.locator('.usage-controls').getByRole('button', { name: '清除筛选', exact: true }).click();
  const requestUrl = await nextStrictUsageUrl(observation, previousCount);
  assert.equal(requestUrl.searchParams.has('status'), false);
  assert.equal(requestUrl.searchParams.has('upstream_account_id'), false);
  assert.equal(requestUrl.searchParams.has('model'), false);
  assert.equal(requestUrl.searchParams.has('key_id'), false);
  assert.equal(requestUrl.searchParams.has('protocol'), false);
  assert.equal(requestUrl.searchParams.has('error_code'), false);
  await page.getByRole('tab', { name: /^(总览|Overview)$/ }).click();
  await assertExactText(metric(page, '请求数'), String(expectedRequests));
}

export async function openUsageFilters(page: Page): Promise<void> {
  const disclosure = page.locator('details.usage-filter-disclosure');
  if (await disclosure.getAttribute('open') === null) await disclosure.locator('summary').click();
  await eventually(async () => assert.notEqual(await disclosure.getAttribute('open'), null));
}

export function usageMetrics(overrides: Record<string, unknown> = {}) {
  return {
    requests: 111_227,
    success: 111_226,
    failed: 1,
    input_tokens: 100_000_000,
    output_tokens: 111_227,
    cached_input_tokens: 0,
    cache_write_tokens: 0,
    generation_units: 0,
    avg_duration_ms: 18.5,
    p95_duration_ms: 25,
    costs: [{ currency: 'CNY', cost: '2.5' }, { currency: 'USD', cost: '1.25' }],
    ...overrides,
  };
}

export function localizationUsageFixture() {
  const bucketStart = Date.UTC(2026, 7, 16, 12);
  const summary = usageMetrics({ cache_write_tokens: 1_000_000_000_000, generation_units: 12_345 });
  return {
    from_created_at: bucketStart,
    to_created_at: bucketStart + 3_600_000 - 1,
    granularity: 'hour',
    time_zone: 'UTC',
    p95_is_approximate: true,
    p95_method: 'fixed_histogram_upper_bound_capped_60000ms',
    upstream_grouping: 'stable_account',
    summary,
    generation_units_by_modality: [{ modality: 'image', currency: 'USD', units: 12_345 }],
    generation_units_by_billing_unit: [{ billing_unit: 'image', currency: 'USD', units: 12_345 }],
    time_series: [{ bucket_start: bucketStart, ...summary }],
    by_model: [{ id: model, label: model, ...summary }],
    by_key: [{ id: runtime.requireSeed().clientKeyId, label: 'Browser E2E credential', ...summary }],
    by_session: [{ id: 'unlinked:browser-e2e', label: 'Unlinked', key_id: runtime.requireSeed().clientKeyId, key_alias: 'Browser E2E credential', unlinked: true, ...summary }],
    by_upstream: [{ id: runtime.requireSeed().upstreamId, label: runtime.requireSeed().upstreamName, ...summary }],
    by_protocol: [{ id: 'openai', label: 'openai', ...summary }],
    by_status: [
      { id: 'success', label: 'success', ...usageMetrics({ requests: 111_226, success: 111_226, failed: 0, cache_write_tokens: 1_000_000_000_000 }) },
      { id: 'error', label: 'failed', ...usageMetrics({ requests: 1, success: 0, failed: 1 }) },
    ],
    errors: [{ id: 'http_429', label: 'http_429', ...usageMetrics({ requests: 1, success: 0, failed: 1 }) }],
    heatmap: [{ hour_of_week: 12, ...summary }],
  };
}

export function strictDimensionUsageFixture(query: URLSearchParams) {
  const seed = runtime.requireSeed();
  const status = query.get('status');
  const upstream = query.get('upstream_account_id');
  const errorCode = query.get('error_code');
  let requests = 17;
  let success = 12;
  let failed = 5;
  let statuses = [
    { id: 'success', label: 'success', ...dimensionUsageMetrics(12, 12, 0) },
    { id: 'error', label: 'failed', ...dimensionUsageMetrics(5, 0, 5) },
  ];
  let upstreams = [
    { id: seed.upstreamId, label: seed.upstreamName, ...dimensionUsageMetrics(11, 8, 3) },
    { id: 'unassigned', label: 'Unassigned', ...dimensionUsageMetrics(6, 4, 2) },
  ];
  if (status === 'error') {
    requests = 5; success = 0; failed = 5;
    statuses = [{ id: 'error', label: 'failed', ...dimensionUsageMetrics(5, 0, 5) }];
    upstreams = [
      { id: seed.upstreamId, label: seed.upstreamName, ...dimensionUsageMetrics(3, 0, 3) },
      { id: 'unassigned', label: 'Unassigned', ...dimensionUsageMetrics(2, 0, 2) },
    ];
  } else if (status === 'success') {
    requests = 12; success = 12; failed = 0;
    statuses = [{ id: 'success', label: 'success', ...dimensionUsageMetrics(12, 12, 0) }];
    upstreams = [
      { id: seed.upstreamId, label: seed.upstreamName, ...dimensionUsageMetrics(8, 8, 0) },
      { id: 'unassigned', label: 'Unassigned', ...dimensionUsageMetrics(4, 4, 0) },
    ];
  } else if (errorCode === 'strict_fixture_error') {
    requests = 5; success = 0; failed = 5;
    statuses = [{ id: 'error', label: 'failed', ...dimensionUsageMetrics(5, 0, 5) }];
    upstreams = [
      { id: seed.upstreamId, label: seed.upstreamName, ...dimensionUsageMetrics(3, 0, 3) },
      { id: 'unassigned', label: 'Unassigned', ...dimensionUsageMetrics(2, 0, 2) },
    ];
  } else if (errorCode) {
    requests = 0; success = 0; failed = 0;
    statuses = [];
    upstreams = [];
  } else if (upstream === 'unassigned') {
    requests = 6; success = 4; failed = 2;
    statuses = [
      { id: 'success', label: 'success', ...dimensionUsageMetrics(4, 4, 0) },
      { id: 'error', label: 'failed', ...dimensionUsageMetrics(2, 0, 2) },
    ];
    upstreams = [{ id: 'unassigned', label: 'Unassigned', ...dimensionUsageMetrics(6, 4, 2) }];
  } else if (upstream) {
    requests = 11; success = 8; failed = 3;
    statuses = [
      { id: 'success', label: 'success', ...dimensionUsageMetrics(8, 8, 0) },
      { id: 'error', label: 'failed', ...dimensionUsageMetrics(3, 0, 3) },
    ];
    upstreams = [{ id: seed.upstreamId, label: seed.upstreamName, ...dimensionUsageMetrics(11, 8, 3) }];
  }
  const summary = dimensionUsageMetrics(requests, success, failed);
  const from = Number(query.get('from_created_at'));
  const to = Number(query.get('to_created_at'));
  const bucketStart = Math.floor((Number.isSafeInteger(from) ? from : Date.now()) / 3_600_000) * 3_600_000;
  return {
    from_created_at: Number.isSafeInteger(from) ? from : bucketStart,
    to_created_at: Number.isSafeInteger(to) ? to : bucketStart + 3_600_000 - 1,
    granularity: 'hour',
    time_zone: 'UTC',
    p95_is_approximate: true,
    p95_method: 'fixed_histogram_upper_bound_capped_60000ms',
    upstream_grouping: 'stable_account',
    summary,
    time_series: [{ bucket_start: bucketStart, ...summary }],
    by_model: [{ id: model, label: model, ...summary }],
    by_key: [{ id: seed.clientKeyId, label: 'Browser E2E credential', ...summary }],
    by_session: [{ id: 'unlinked:browser-e2e', label: 'Unlinked', key_id: seed.clientKeyId, key_alias: 'Browser E2E credential', unlinked: true, ...summary }],
    by_upstream: upstreams,
    by_protocol: [{ id: 'openai', label: 'openai', ...summary }],
    by_status: statuses,
    errors: failed ? [{ id: 'strict_fixture_error', label: 'strict_fixture_error', ...dimensionUsageMetrics(failed, 0, failed) }] : [],
    heatmap: [{ hour_of_week: 12, ...summary }],
  };
}

export function dimensionUsageMetrics(requests: number, success: number, failed: number) {
  return usageMetrics({
    requests,
    success,
    failed,
    input_tokens: requests * 10,
    output_tokens: requests * 2,
    avg_duration_ms: requests ? 20 : null,
    p95_duration_ms: requests ? 40 : null,
    costs: requests ? [{ currency: 'USD', cost: (requests / 100).toFixed(2) }] : [],
  });
}

export function emptyUsageFixture() {
  const empty = usageMetrics({
    requests: 0,
    success: 0,
    failed: 0,
    input_tokens: 0,
    output_tokens: 0,
    avg_duration_ms: null,
    p95_duration_ms: null,
    costs: [],
  });
  return {
    from_created_at: Date.UTC(2026, 7, 16),
    to_created_at: Date.UTC(2026, 7, 16, 23, 59, 59, 999),
    granularity: 'hour',
    time_zone: 'UTC',
    p95_is_approximate: true,
    p95_method: 'fixed_histogram_upper_bound_capped_60000ms',
    upstream_grouping: 'stable_account',
    summary: empty,
    time_series: [],
    by_model: [],
    by_key: [],
    by_session: [],
    by_upstream: [],
    by_protocol: [],
    by_status: [],
    errors: [],
    heatmap: [],
  };
}

export async function assertNoHorizontalOverflow(page: Page): Promise<void> {
  const layout = await page.evaluate(() => {
    const viewport = document.documentElement.clientWidth;
    const overflowers = Array.from(document.querySelectorAll<HTMLElement>('body *')).map((element) => {
      const bounds = element.getBoundingClientRect();
      return {
        element: `${element.tagName.toLowerCase()}${element.id ? `#${element.id}` : ''}${Array.from(element.classList).map((name) => `.${name}`).join('')}`,
        left: Math.round(bounds.left),
        right: Math.round(bounds.right),
        width: Math.round(bounds.width),
        scrollWidth: element.scrollWidth,
      };
    }).filter((item) => item.left < 0 || item.right > viewport || item.width > viewport).slice(0, 12);
    return {
      viewport,
      innerWidth: window.innerWidth,
      visualViewport: window.visualViewport?.width,
      mobile480: window.matchMedia('(width <= 480px)').matches,
      mobile900: window.matchMedia('(width <= 900px)').matches,
      document: document.documentElement.scrollWidth,
      overflowers,
    };
  });
  assert.ok(layout.document <= layout.viewport, JSON.stringify(layout, null, 2));
}
