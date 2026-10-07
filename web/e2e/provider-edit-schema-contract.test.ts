import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import type { RJSFSchema } from '@rjsf/utils';
import { localizeSchema } from '../src/i18n.js';
import { providerEditSchema } from '../src/operator/providerEditSchema.js';
import { connectionSchema } from '../src/operator/upstreamConnectionPolicy.js';
import { codexTransportPolicySchema, providerEditShape } from './fixtures/provider-edit-shapes.js';

function transportPolicy(schema: RJSFSchema): RJSFSchema {
  const config = schema.properties?.config;
  assert.ok(config && typeof config === 'object');
  const policy = config.properties?.transport_policy;
  assert.ok(policy && typeof policy === 'object');
  return policy;
}

function withoutPresentation(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(withoutPresentation);
  if (!value || typeof value !== 'object') return value;
  return Object.fromEntries(Object.entries(value)
    .filter(([key]) => key !== 'title' && key !== 'description')
    .map(([key, nested]) => [key, withoutPresentation(nested)]));
}

test('transport fixture retains the current catalog descriptions, defaults and bounds', async () => {
  const source = await readFile(new URL('../../src/provider/catalog.rs', import.meta.url), 'utf8');
  const match = source.match(/codex\.config_schema\["properties"\]\["transport_policy"\] = json!\((\{[\s\S]*?\n        \})\);/);
  assert.ok(match, 'locate the Codex transport policy JSON');
  assert.deepEqual(codexTransportPolicySchema, JSON.parse(match[1]));
});

test('connection, localization and edit composition translate transport copy without changing schema semantics', () => {
  const registry: RJSFSchema = { type: 'object', additionalProperties: true, ...providerEditShape('csil')!.schema };
  const original = structuredClone(registry);
  const connected: RJSFSchema = { type: 'object', properties: { config: connectionSchema(registry, 'Endpoint hint') } };
  const connectedSnapshot = structuredClone(connected);
  const expectedEnglish = structuredClone(connected);
  const englishPolicy = transportPolicy(expectedEnglish);
  for (const [key, title] of Object.entries({
    dispatch_max_in_flight: 'Maximum dispatch concurrency',
    dispatch_max_queued: 'Dispatch queue capacity',
    dispatch_queue_timeout_millis: 'Dispatch queue timeout (ms)',
  })) {
    const field = englishPolicy.properties?.[key];
    assert.ok(field && typeof field === 'object');
    field.title = title;
  }
  const labels = {
    version: '策略版本', connect_timeout_millis: '连接超时（毫秒）', read_timeout_millis: '读取无数据超时（毫秒）',
    request_timeout_millis: '请求总超时（毫秒）', memory_admission_wait_millis: '内存排队超时（毫秒）',
    dispatch_max_in_flight: '调度最大并发数', dispatch_max_queued: '调度队列容量', dispatch_queue_timeout_millis: '调度排队超时（毫秒）',
    max_sse_event_bytes: '最大 SSE 事件（字节）', max_sse_framed_bytes: '单个网络分片最大帧数据量（字节）', max_sse_terminal_hold_bytes: '终止事件最大暂存量（字节）',
    candidate_attempts: '候选尝试次数', failover_deadline_millis: '故障切换时限（毫秒）', connect_attempts: '连接尝试次数',
    connect_retry_delay_millis: '连接重试间隔（毫秒）', shared_probe_attempts: '共享探测次数',
    chat_controls: 'Chat 控制策略', responses_output_limits: 'Responses 输出限制',
  };
  const policyHelp: Record<string, string> = {
    chat_controls: '默认策略（provider_default）会校验并移除不受支持的 Chat 采样和输出限制参数；上游使用自身默认值，不保证请求指定的采样设置或词元上限。显式选择严格策略（strict）时，仅接受中性的采样值，并拒绝输出限制提示。额度预留仍使用可信的模型上限。',
    responses_output_limits: '默认策略（provider_default）会校验并移除客户端的输出限制提示；上游使用自身默认值，不强制执行请求指定的词元上限。由于 Codex OAuth 无法保证该限制，显式选择严格策略（strict）时会拒绝任何输出限制提示。额度始终按可信的模型上限预留，并按观测到的实际用量结算。',
  };
  const existingEditHelp: Record<string, string> = {
    connect_timeout_millis: '建立连接的等待时限，必须小于请求总超时。',
    read_timeout_millis: '收到响应头后等待首段内容，以及后续相邻内容之间允许的最长无数据时间。',
    request_timeout_millis: '从首次发送到完整接收响应的总时限，包含系统允许的重放。',
    dispatch_max_in_flight: '此账号每个代理出口的可选请求并发上限。默认值 0 表示不添加派发限制，也不使用派发队列。正数会限制正在执行的请求（包括流式响应）；调低已设置的上限时，现有请求继续完成。',
    dispatch_max_queued: '仅在派发并发上限大于 0 时生效。设置可排队等待的请求数；0 表示并发满时立即拒绝新请求。',
    dispatch_queue_timeout_millis: '仅在派发并发上限大于 0 时生效。请求排队超过此时限会返回 503；该请求尚未派发或计费。',
    max_sse_event_bytes: '单个上游 SSE 事件允许保留的最大字节数；Responses 终止事件可能包含完整响应对象。',
    max_sse_framed_bytes: '单个网络分片可产出的完整 SSE 帧总字节数，必须不小于单事件上限。',
    max_sse_terminal_hold_bytes: '等待 EOF 验证时可暂存的终止事件总字节数，必须介于单事件上限与单分片 framing 上限之间。',
  };
  for (const locale of ['zh-CN', 'en'] as const) {
    const create = localizeSchema(connected, locale);
    const createSnapshot = structuredClone(create);
    const edit = providerEditSchema(create, locale);
    if (locale === 'en') {
      assert.deepEqual(create, expectedEnglish, 'English titles and help are unchanged; untitled dispatch fields gain English labels');
    }
    for (const projected of [create, edit]) {
      assert.deepEqual(withoutPresentation(projected), withoutPresentation(connected), 'retain all property keys, required fields, defaults, bounds, enums and additional-property rules');
      const policy = transportPolicy(projected);
      assert.equal(policy.title, locale === 'zh-CN' ? '运行时传输策略' : 'Runtime transport policy');
      assert.equal(policy.description, projected === create
        ? codexTransportPolicySchema.description
        : locale === 'zh-CN'
          ? '单位见各字段；这些设置影响该账号的所有请求。'
          : 'Units are shown per field. These settings affect every request using this account.');
      for (const [key, source] of Object.entries(codexTransportPolicySchema.properties)) {
        const field = policy.properties?.[key];
        assert.ok(field && typeof field === 'object');
        if (locale === 'zh-CN') assert.equal(field.title, labels[key as keyof typeof labels]);
        else assert.equal(field.title, (englishPolicy.properties![key] as RJSFSchema).title, `${key} English title`);
        if ('description' in source) {
          const expectedHelp = locale === 'zh-CN'
            ? policyHelp[key] ?? (projected === edit ? existingEditHelp[key] : undefined) ?? source.description
            : source.description;
          assert.equal(field.description, expectedHelp, `${locale} ${key} help changes only for the two newly localized policies`);
        }
      }
      if (locale === 'zh-CN') {
        const chat = policy.properties!.chat_controls as RJSFSchema;
        const responses = policy.properties!.responses_output_limits as RJSFSchema;
        assert.match(chat.description!, /provider_default.*校验并移除.*采样和输出限制.*自身默认值.*不保证.*采样设置或词元上限.*strict.*仅接受中性的采样值.*拒绝输出限制.*可信的模型上限/u);
        assert.match(responses.description!, /provider_default.*校验并移除.*输出限制.*自身默认值.*不强制执行.*词元上限.*strict.*拒绝任何输出限制.*可信的模型上限预留.*实际用量结算/u);
      }
    }
    assert.deepEqual(create, createSnapshot, 'edit presentation does not mutate the localized create schema');
  }
  assert.deepEqual(registry, original, 'registry schema is not mutated');
  assert.deepEqual(connected, connectedSnapshot, 'localization does not mutate the connection schema');
});

test('partial transport schemas keep absent fields absent and unknown extensions intact in both locales', () => {
  const unknown = { type: 'string' as const, title: 'Vendor extension', description: 'Unmapped vendor help', enum: ['vendor_default', 'vendor_strict'], default: 'vendor_default' };
  const registry: RJSFSchema = { type: 'object', additionalProperties: true, properties: {
    transport_policy: { type: 'object', required: ['connect_attempts'], additionalProperties: true,
      properties: { connect_attempts: { type: 'integer', minimum: 1, maximum: 4, default: 2 }, vendor_extension: unknown } },
    future_setting: unknown,
  } };
  const original = structuredClone(registry);
  for (const locale of ['zh-CN', 'en'] as const) {
    const connected: RJSFSchema = { properties: { config: connectionSchema(registry, 'Endpoint hint') } };
    const create = localizeSchema(connected, locale);
    for (const projected of [create, providerEditSchema(create, locale)]) {
      assert.deepEqual(withoutPresentation(projected), withoutPresentation(connected));
      assert.deepEqual(Object.keys(transportPolicy(projected).properties!), ['connect_attempts', 'vendor_extension']);
      assert.deepEqual(transportPolicy(projected).properties!.vendor_extension, unknown);
      assert.deepEqual((projected.properties!.config as RJSFSchema).properties!.future_setting, unknown);
    }
  }
  assert.deepEqual(registry, original);
});

test('provider presentation changes only copy, preserving unknown schemas and constraints', () => {
  const schema = { type: 'object' as const, properties: { config: { type: 'object' as const,
    required: ['network_scope', 'reservation_token_bounds'], additionalProperties: true,
    properties: {
      network_scope: { type: 'string' as const, const: 'public', default: 'public' },
      reservation_token_bounds: { type: 'object' as const, additionalProperties: { type: 'integer' as const, minimum: 1 }, description: 'internal copy' },
      future: { type: 'object' as const, properties: { opaque: { type: 'string' as const, default: 'synthetic-default' } }, additionalProperties: true },
    },
  } } };
  const original = structuredClone(schema);
  for (const locale of ['zh-CN', 'en']) {
    const projected = providerEditSchema(schema, locale);
    const config = projected.properties!.config;
    assert.ok(config && typeof config === 'object');
    assert.deepEqual(config.required, original.properties.config.required);
    assert.equal(config.additionalProperties, true);
    assert.deepEqual(config.properties!.future, original.properties.config.properties.future);
    const bounds = config.properties!.reservation_token_bounds;
    assert.ok(bounds && typeof bounds === 'object');
    assert.deepEqual(bounds.additionalProperties, { type: 'integer', minimum: 1 });
    assert.notEqual(bounds.description, 'internal copy');
  }
  assert.deepEqual(schema, original, 'do not mutate the provider registry schema');
  const implicit = providerEditSchema({ properties: { config: { type: 'object' } } }, 'zh-CN');
  const explicitDeny = providerEditSchema({ properties: { config: { type: 'object', additionalProperties: false } } }, 'zh-CN');
  assert.equal((implicit.properties!.config as { additionalProperties: boolean }).additionalProperties, true);
  assert.equal((explicitDeny.properties!.config as { additionalProperties: boolean }).additionalProperties, false);
});
