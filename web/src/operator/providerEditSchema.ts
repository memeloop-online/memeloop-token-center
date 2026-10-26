import type { RJSFSchema } from '@rjsf/utils';

/** Presentation only: never remove fields, defaults, constraints or unknown data. */
export function providerEditSchema(schema: RJSFSchema, locale: string): RJSFSchema {
  const result = structuredClone(schema);
  const zh = locale.startsWith('zh');
  const properties = result.properties?.config;
  if (!properties || typeof properties !== 'object') return result;
  // JSON Schema allows extras when this keyword is omitted. RJSF only exposes
  // their controls when it is explicit; honor that same permission in the UI.
  if (properties.additionalProperties === undefined) properties.additionalProperties = true;
  const copy: Record<string, [string, string, string, string]> = {
    network_scope: ['网络访问范围', 'Network access scope', '公网服务选择 public，私网服务选择 private；连接私网服务需要全局操作员凭据。', 'Choose public for an internet service or private for a private network service. Private destinations require a global operator credential.'],
    reservation_token_bounds: ['模型词元预留上限', 'Model token reservation bounds', '按准确模型名称设置请求预算用的词元预留。模型实际输出上限以模型目录为准。', 'Token reservation per exact model name, used for the request budget. The model output limit comes from the model catalog.'],
    transport_policy: ['运行时传输策略', 'Runtime transport policy', '单位见各字段；这些设置影响该账号的所有请求。', 'Units are shown per field. These settings affect every request using this account.'],
    timeout_seconds: ['请求超时（秒）', 'Request timeout (seconds)', '设置等待上游完成请求的最长时间；不确定时保留默认值。', 'Set how long to wait for an upstream request to finish. Keep the default if unsure.'],
    provider_asset_reads_repeatable: ['生成文件可重复读取', 'Generated files allow repeated downloads', '仅在生成文件的下载地址可重复使用时启用；一次性下载地址请保持关闭。', 'Enable only when generated file URLs allow repeated downloads. Leave off for single-use URLs.'],
    responses_compact_v2_bridge: ['Responses 压缩接口', 'Responses compaction endpoint', '仅在提供商支持 POST /v1/responses/compact 时启用，将 Responses 压缩请求交给该接口处理。', 'Enable only if your provider supports POST /v1/responses/compact, to send Responses compaction requests to that endpoint.'],
    responses_via_chat_compaction: ['Responses 上下文摘要', 'Responses context summaries', '允许用此账号生成长对话的摘要，供后续对话继续使用。摘要可能遗漏细节；仅在提供商能可靠生成摘要时启用，不确定时保持关闭。', 'Allow this account to summarize long conversations so they can continue. Summaries can omit details. Enable only when the provider produces reliable summaries; leave off if unsure.'],
  };
  for (const [name, [zhTitle, enTitle, zhHint, enHint]] of Object.entries(copy)) {
    const field = properties.properties?.[name];
    if (field && typeof field === 'object') {
      field.title = zh ? zhTitle : enTitle;
      field.description = zh ? zhHint : enHint;
    }
  }
  const policy = properties.properties?.transport_policy;
  if (zh && policy && typeof policy === 'object') {
    for (const [name, description] of Object.entries({
      connect_timeout_millis: '此数值仍须小于请求总超时。Codex 转发在上游返回、客户端取消或连接失败时结束。代理是否可达，看代理组上的独立探测。',
      read_timeout_millis: '一段时间没有新内容时，Codex 转发保持连接。响应在上游结束、客户端取消或连接本身失败时结束。',
      request_timeout_millis: 'Codex 转发（含压缩）没有总时限。请求在上游返回、客户端取消或连接本身失败时结束。',
      dispatch_max_in_flight: '此账号每个代理出口的可选请求并发上限。默认值 0 表示不添加派发限制，也不使用派发队列。正数会限制正在执行的请求（包括流式响应）；调低已设置的上限时，现有请求继续完成。',
      dispatch_max_queued: '仅在派发并发上限大于 0 时生效。设置可排队等待的请求数；0 表示并发满时立即拒绝新请求。',
      dispatch_queue_timeout_millis: '仅在派发并发上限大于 0 时生效。请求排队超过此时限会返回 503；该请求尚未派发或计费。',
      max_sse_event_bytes: '单个上游 SSE 事件允许保留的最大字节数；Responses 终止事件可能包含完整响应对象。',
      max_sse_framed_bytes: '单个网络分片可产出的完整 SSE 帧总字节数，必须不小于单事件上限。',
      max_sse_terminal_hold_bytes: '等待 EOF 验证时可暂存的终止事件总字节数，必须介于单事件上限与单分片 framing 上限之间。',
    })) {
      const field = policy.properties?.[name];
      if (field && typeof field === 'object') field.description = description;
    }
  }
  return result;
}

export function providerConfigSchema(schema: RJSFSchema, locale: string): RJSFSchema {
  return providerEditSchema({ properties: { config: schema } }, locale).properties!.config as RJSFSchema;
}
