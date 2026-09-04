import assert from 'node:assert/strict';
import { Then, When } from '@cucumber/cucumber';
import type { Locator, Page } from 'playwright';
import { eventually, model, requestJson, runtime, tenant } from '../support/runtime.js';
import type { DogfoodWorld } from '../support/world.js';
import { appPreferenceControls, openAppRoute } from './app-route.support.js';

interface SessionObservation {
  liveRequests: Promise<Response>[];
  sessionListRequests: string[];
  detailRequests: string[];
  baselineSessionListRequests: number;
}

const observations = new WeakMap<DogfoodWorld, SessionObservation>();
const sharedCreatedAt = Date.UTC(2026, 7, 21, 12);

function conversationRequest(index: number, currency: 'USD' | 'CNY') {
  return {
    request_id: `session-request-${String(index).padStart(3, '0')}`,
    created_at: sharedCreatedAt,
    protocol: 'openai',
    model,
    status_code: index === 7 ? 429 : 200,
    duration_ms: 20 + index,
    input_tokens: index * 10,
    output_tokens: index * 2,
    cost: index % 2 ? '1.25' : '2.50',
    currency,
    error_code: index === 7 ? 'session_fixture_error' : null,
    source: 'live',
    provenance: 'native',
    unlinked: false,
    execution: index === 8 ? undefined : {
      session_name: '发布试用',
      trace_id: '4bf92f3577b34da6a3ce929d0e0e4736',
      span_id: `span-${index}`,
      parent_span_id: index === 1 ? null : `span-${index - 1}`,
      agent_id: index % 3 === 0 ? 'research-worker' : 'codex-root',
      parent_agent_id: index % 3 === 0 ? 'codex-root' : null,
      task_kind: index % 2 ? 'interactive' : 'background',
      labels: { workflow: 'release', environment: 'api2-trial' },
      source: 'declared',
    },
    structure: {
      session_id: 'codex-session-browser',
      turn_id: `codex-turn-${index}`,
      parent_turn_id: index === 1 ? null : `codex-turn-${index - 1}`,
      response_id: `resp-${index}`,
      branch_id: index === 6 ? 'research-branch' : null,
      compaction: index === 7,
      client_name: 'Codex CLI',
      source: 'client_protocol',
    },
  };
}

function detailFixture(older: boolean) {
  const requests = older
    ? [conversationRequest(7, 'USD'), conversationRequest(8, 'CNY')]
    : Array.from({ length: 7 }, (_, index) => conversationRequest(index + 1, index % 2 ? 'CNY' : 'USD'));
  const relations = ['continues', 'retry', 'edit', 'branch', 'compacts', 'subagent'] as const;
  return {
    session_id: 'session-browser-detail',
    cluster_id: 'cluster-browser-detail',
    unlinked: false,
    requests,
    edges: older ? [] : [
      ...relations.map((relation, index) => ({
        from_request_id: index === 0 ? null : requests[index - 1].request_id,
        to_request_id: requests[index].request_id,
        relation,
        confidence: 0.9,
        evidence: { source: 'cucumber' },
      })),
      {
        from_request_id: requests[0].request_id,
        to_request_id: requests.at(-1)!.request_id,
        relation: 'candidate',
        confidence: 0.55,
        evidence: { source: 'cucumber-candidate' },
      },
    ],
    has_more: !older,
    next_cursor: older ? null : { before_created_at: sharedCreatedAt, before_request_id: requests.at(-1)!.request_id },
    edges_truncated: false,
  };
}

async function visible(locator: Locator) {
  await eventually(async () => assert.equal(await locator.isVisible(), true));
}

async function noHorizontalOverflow(page: Page) {
  const overflow = await page.evaluate(() => ({
    document: document.documentElement.scrollWidth - document.documentElement.clientWidth,
    body: document.body.scrollWidth - document.body.clientWidth,
  }));
  assert.ok(overflow.document <= 1 && overflow.body <= 1, `horizontal overflow: ${JSON.stringify(overflow)}`);
}

When('管理员打开实时会话聚合', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const observation: SessionObservation = { liveRequests: [], sessionListRequests: [], detailRequests: [], baselineSessionListRequests: 0 };
  observations.set(this, observation);
  page.on('request', (request) => {
    const url = new URL(request.url());
    if (url.pathname === '/internal/v1/sessions') observation.sessionListRequests.push(url.toString());
    else if (url.pathname.startsWith('/internal/v1/sessions/')) observation.detailRequests.push(url.toString());
  });
  await openAppRoute(page, 'operator', 'sessions');
  await visible(page.getByRole('heading', { name: '最近会话与未关联请求', exact: true }));
  await visible(page.locator('.session-live-state'));
});

Then('同时间戳会话详情分页无重复遗漏且 USD 与 CNY 分行', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await page.route('**/internal/v1/sessions/**', async (route) => {
    const url = new URL(route.request().url());
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify(detailFixture(url.searchParams.has('before_request_id'))),
    });
  });
  const firstCard = page.locator('.session-card').first();
  await visible(firstCard);
  await firstCard.getByRole('button', { name: /^打开 / }).click();
  const drawer = page.getByRole('dialog');
  await visible(drawer);
  await eventually(async () => assert.equal(await drawer.locator('tbody tr').count(), 7));
  assert.match(await drawer.textContent() ?? '', /US\$|\$/);
  assert.match(await drawer.textContent() ?? '', /CN¥|¥/);
  await drawer.getByRole('button', { name: '加载更早请求', exact: true }).click();
  await eventually(async () => assert.equal(await drawer.locator('tbody tr').count(), 8));
  const cursorUrl = new URL(observations.get(this)!.detailRequests.at(-1)!);
  assert.equal(cursorUrl.searchParams.get('before_created_at'), String(sharedCreatedAt));
  assert.equal(cursorUrl.searchParams.get('before_request_id'), 'session-request-007');
  const rows = await drawer.locator('tbody tr').allTextContents();
  assert.equal(new Set(rows).size, 8, 'same-timestamp cursor must not duplicate or omit requests');
});

Then('六类可靠关系、候选关系、未关联请求和语义执行图被明确区分', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const drawer = page.getByRole('dialog');
  const text = await drawer.textContent() ?? '';
  assert.match(text, /语义执行图.*发布试用/s);
  assert.match(text, /codex-root.*research-worker/s);
  assert.match(text, /interactive.*background/s);
  assert.match(text, /结构关联/);
  assert.match(text, /codex-session-browser/);
  assert.match(text, /耗时条形图/);
  assert.match(text, /任务类型费用/);
  assert.ok(await drawer.locator('.execution-lane.inferred').count() > 0, 'protocol-only Codex request must remain visible as inferred structure');
  assert.match(text, /代理费用/);
  assert.match(text, /US\$|\$/);
  assert.match(text, /CN¥|¥/);
  for (const relation of ['继续', '重试', '编辑', '分支', '压缩', '子代理']) assert.match(text, new RegExp(relation));
  for (const sentence of ['延续了', '是对', '修改了', '创建了分支', '上下文后继续', '派生的子代理请求']) {
    assert.match(text, new RegExp(sentence), `missing natural-language relationship: ${sentence}`);
  }
  const candidate = drawer.locator('.candidate-edges');
  await candidate.locator('summary').click();
  assert.match(await candidate.textContent() ?? '', /不计入会话统计/);
  await drawer.getByRole('button', { name: '关闭', exact: true }).click();
  await appPreferenceControls(page).getByRole('button', { name: 'English', exact: true }).click();
  await page.locator('.session-card').first().getByRole('button', { name: /^Open / }).click();
  const englishDrawer = page.getByRole('dialog');
  await visible(englishDrawer);
  const englishText = await englishDrawer.textContent() ?? '';
  for (const sentence of ['continues the conversation', 'retries', 'edits the input', 'branches from', 'compacting the context', 'subagent request spawned']) {
    assert.match(englishText, new RegExp(sentence), `missing English natural-language relationship: ${sentence}`);
  }
  await englishDrawer.getByRole('button', { name: 'Close', exact: true }).click();
  await appPreferenceControls(page).getByRole('button', { name: '中文', exact: true }).click();
  const seed = runtime.requireSeed();
  await page.route('**/internal/v1/sessions?**', async (route) => {
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({
        generated_at: Date.now(),
        next_cursor: null,
        sessions: [{
          session_id: `unlinked:${seed.clientKeyId}`, cluster_id: null, unlinked: true,
          key_id: seed.clientKeyId, key_alias: 'Browser E2E credential', model, protocol: 'openai',
          last_status: 'success', last_activity_at: Date.now(), active_requests: 0, requests: 1,
          errors: 0, input_tokens: 10, output_tokens: 2, avg_duration_ms: 20,
          costs: [{ currency: 'USD', cost: '0.01' }], archived_only_requests: 0,
          archived_only_errors: 0, archived_only_input_tokens: 0,
          archived_only_output_tokens: 0, archived_only_avg_duration_ms: null,
        }],
      }),
    });
  });
  const controls = page.locator('.session-controls');
  await controls.getByLabel('搜索').fill('unlinked:');
  await controls.getByRole('button', { name: '应用筛选', exact: true }).click();
  const unlinked = page.locator('.session-card').filter({ hasText: '未关联请求' }).first();
  await visible(unlinked);
  assert.match(await unlinked.textContent() ?? '', /未关联会话/);
});

When('连续新请求进入活跃状态并分别完成为成功和错误', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const seed = runtime.requireSeed();
  const controls = page.locator('.session-controls');
  await controls.getByLabel('会话状态').selectOption('active');
  await controls.getByRole('button', { name: '应用筛选', exact: true }).click();
  const observation = observations.get(this)!;
  observation.baselineSessionListRequests = observation.sessionListRequests.length;
  const sendCall = (index: number) => fetch(new URL('/v1/chat/completions', page.url()), {
    method: 'POST',
    headers: {
      Authorization: `Bearer ${seed.clientCredential}`,
      'Content-Type': 'application/json',
      'X-Codex-Session-Id': 'browser-codex-semantic-session',
      'X-MTC-Turn-Id': `browser-turn-${index}`,
      ...(index > 0 ? { 'X-MTC-Parent-Turn-Id': `browser-turn-${index - 1}` } : {}),
      'X-MTC-Session-Name': 'Codex release dogfood',
      'X-MTC-Agent-Id': index === 1 ? 'codex-worker' : 'codex-root',
      ...(index === 1 ? { 'X-MTC-Parent-Agent-Id': 'codex-root' } : {}),
      'X-MTC-Task-Kind': index % 2 ? 'background' : 'interactive',
      'X-MTC-Session-Labels': JSON.stringify({ workflow: 'release', environment: 'browser-e2e' }),
      traceparent: `00-4bf92f3577b34da6a3ce929d0e0e4736-${String(index + 1).padStart(16, '0')}-01`,
    },
    body: JSON.stringify({
      model,
      messages: [{ role: 'user', content: `force session active ${index % 2 ? 'error' : 'success'} ${index}` }],
      max_tokens: 32,
    }),
  });
  // Put both successful requests in the two available concurrency slots before
  // the rate-limit probes arrive. Once either probe receives a 429, the sole
  // account enters cooldown and both failed client requests correctly collapse
  // to the public 503/Retry-After contract instead of leaking account state.
  const calls = [sendCall(0), sendCall(2), ...[1, 3].map(async (index) => {
    await new Promise((resolve) => setTimeout(resolve, 50));
    return sendCall(index);
  })];
  observation.liveRequests = calls;
  await eventually(async () => {
    await visible(page.locator('.session-card').first());
    assert.match(await page.locator('.session-card').first().textContent() ?? '', /活跃/);
  }, 5_000, 'active session was not visible');
});

Then('Codex 上报的会话名称、代理层级和任务分类进入真实语义视图', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const controls = page.locator('.session-controls');
  await controls.getByLabel('会话状态').selectOption('');
  await controls.getByLabel('搜索').fill('Codex release dogfood');
  await controls.getByRole('button', { name: '应用筛选', exact: true }).click();
  const card = page.locator('.session-card').filter({ hasText: 'Codex release dogfood' }).first();
  await visible(card);
  await card.getByRole('button', { name: /^打开 / }).click();
  const drawer = page.getByRole('dialog');
  await visible(drawer);
  const text = await drawer.textContent() ?? '';
  assert.match(text, /语义执行图.*Codex release dogfood/s);
  assert.match(text, /codex-root/);
  assert.match(text, /codex-worker/);
  assert.match(text, /interactive.*background/s);
  assert.match(text, /4bf92f3577b34da6a3ce929d0e0e4736/);
  assert.match(text, /browser-e2e/);
  assert.match(text, /browser-codex-semantic-session/);
  assert.match(text, /耗时条形图/);
  await drawer.getByRole('button', { name: '关闭', exact: true }).click();
});

Then('连续事件期间会话计数有界前进且活跃筛选移除已完成会话', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const observation = observations.get(this)!;
  await eventually(
    () => assert.ok(observation.sessionListRequests.length > observation.baselineSessionListRequests),
    1_400,
    'coalesced refresh starved under continuous events',
  );
  const responses = await Promise.all(observation.liveRequests);
  assert.deepEqual(responses.map((response) => response.status).sort(), [200, 200, 503, 503]);
  for (const response of responses.filter((candidate) => candidate.status === 503)) {
    assert.equal(response.headers.get('retry-after'), '1');
  }
  await eventually(async () => assert.equal(await page.locator('.session-card').count(), 0), 5_000, 'completed session remained in the active filter');
  const refreshes = observation.sessionListRequests.length - observation.baselineSessionListRequests;
  assert.ok(refreshes >= 1 && refreshes <= 6, `continuous event refresh count was not bounded: ${refreshes}`);
  assert.match(await page.locator('.session-result-count').textContent() ?? '', /0/);
});

Then('服务端错误筛选返回含错误的聚合结果', async function () {
  const seed = runtime.requireSeed();
  const response = await requestJson<{ sessions: Array<{ requests: number; errors: number }>; next_cursor: unknown }>(
    `/internal/v1/sessions?tenant_external_id=${encodeURIComponent(tenant)}&state=has_errors&limit=1`,
    { credential: seed.serviceCredential },
  );
  assert.equal(response.sessions.length, 1);
  assert.ok(response.sessions[0].requests >= response.sessions[0].errors);
  assert.ok(response.sessions[0].errors >= 1);
});

Then('其他凭据事件和无事件重连不会污染已打开的会话', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const seed = runtime.requireSeed();
  const controls = page.locator('.session-controls');
  await controls.getByRole('button', { name: '清除筛选', exact: true }).click();
  await visible(page.locator('.session-card').first());
  await page.locator('.session-card').first().getByRole('button', { name: /^打开 / }).click();
  await visible(page.getByRole('dialog'));
  const observation = observations.get(this)!;
  const detailCount = observation.detailRequests.length;
  const otherCredentialResponse = await fetch(new URL('/v1/chat/completions', page.url()), {
    method: 'POST',
    headers: { Authorization: `Bearer ${seed.otherClientCredential}`, 'Content-Type': 'application/json' },
    body: JSON.stringify({ model, messages: [{ role: 'user', content: 'different credential session event' }], max_tokens: 16 }),
  });
  assert.ok([200, 429].includes(otherCredentialResponse.status), `unexpected other-credential status ${otherCredentialResponse.status}`);
  await new Promise((resolve) => setTimeout(resolve, 1_200));
  assert.equal(observation.detailRequests.length, detailCount, 'another credential event refreshed the selected detail');

  await page.getByRole('dialog').getByRole('button', { name: '关闭', exact: true }).click();
  await openAppRoute(page, 'operator', 'usage');
  await page.route('**/internal/v1/request-events**', async (route) => {
    await route.fulfill({ status: 200, contentType: 'text/event-stream', body: ': keepalive\n\n' });
  });
  await openAppRoute(page, 'operator', 'requests');
  await eventually(async () => assert.match(await page.locator('.session-live-state').textContent() ?? '', /正在重新连接/), 4_000);
});

Then('实际轮换后的新凭据保留旧会话历史而旧凭据和其他身份被拒', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  const seed = runtime.requireSeed();
  const before = await requestJson<{ sessions: Array<{ session_id: string; key_id: string }> }>('/self/v1/sessions', {
    credential: seed.clientCredential,
  });
  assert.ok(before.sessions.length > 0, 'rotation continuity needs an existing session');
  const oldCredential = seed.clientCredential;
  const rotated = await requestJson<{ key: string }>(`/internal/v1/keys/${seed.clientKeyId}/rotate`, {
    method: 'POST',
    credential: seed.serviceCredential,
    headers: { 'Idempotency-Key': crypto.randomUUID() },
  });
  const rejected = await fetch(new URL('/self/v1/sessions', page.url()), {
    headers: { Authorization: `Bearer ${oldCredential}` },
  });
  assert.equal(rejected.status, 401);
  const after = await requestJson<{ sessions: Array<{ session_id: string; key_id: string }> }>('/self/v1/sessions', {
    credential: rotated.key,
  });
  assert.deepEqual(after.sessions.map((session) => session.session_id), before.sessions.map((session) => session.session_id));
  assert.ok(after.sessions.every((session) => session.key_id === seed.clientKeyId));
  const other = await requestJson<{ sessions: Array<{ key_id: string }> }>('/self/v1/sessions', {
    credential: seed.otherClientCredential,
  });
  assert.ok(other.sessions.every((session) => session.key_id === seed.otherClientKeyId));
  assert.ok(other.sessions.every((session) => session.key_id !== seed.clientKeyId));
  const forbiddenDetail = await fetch(new URL(
    `/self/v1/sessions/${encodeURIComponent(before.sessions[0].session_id)}`,
    page.url(),
  ), { headers: { Authorization: `Bearer ${seed.otherClientCredential}` } });
  assert.equal(forbiddenDetail.status, 404);

  seed.clientCredential = rotated.key;
  await page.getByRole('button', { name: '清空凭据', exact: true }).click();
  await page.locator('input[type="password"]').fill(rotated.key);
  await page.getByRole('button', { name: '进入', exact: true }).click();
  await openAppRoute(page, 'portal', 'sessions');
  await visible(page.locator('.self-sessions .session-card').first());
  assert.equal((await page.locator('.self-sessions').textContent() ?? '').includes(seed.clientKeyId), false);
});

Then('会话界面支持中英文亮暗主题、键盘和 320 与 375 像素视口', async function (this: DogfoodWorld) {
  const page = this.requirePage();
  await page.setViewportSize({ width: 320, height: 740 });
  await noHorizontalOverflow(page);
  const timeline = page.locator('.self-sessions .session-card').first().getByRole('button', { name: /^打开 / });
  await timeline.focus();
  await page.keyboard.press('Enter');
  await visible(page.getByRole('dialog'));
  await page.getByRole('dialog').getByRole('button', { name: '关闭', exact: true }).click();
  await appPreferenceControls(page).getByRole('button', { name: 'English', exact: true }).click();
  await visible(page.getByRole('heading', { name: 'My sessions and requests', exact: true }));
  await appPreferenceControls(page).getByRole('button', { name: 'Switch to dark theme' }).click();
  assert.equal(await page.locator('html').getAttribute('data-theme'), 'dark');
  await page.setViewportSize({ width: 375, height: 812 });
  await noHorizontalOverflow(page);
  this.assertNoBrowserFailures();
});
