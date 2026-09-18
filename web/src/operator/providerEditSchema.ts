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
    network_scope: ['网络访问范围', 'Network access scope', '控制上游可访问的网络范围；通常保持现有值。', 'Controls the network scope available to this upstream. Usually leave unchanged.'],
    reservation_token_bounds: ['模型词元预留上限', 'Model token reservation bounds', '按准确模型名称设置保守的词元预留值，用于请求预算，不代表模型输出上限。', 'Conservative token reservations per exact model name, used for request budgeting rather than model output limits.'],
    transport_policy: ['超时与重试参数', 'Timeout and retry parameters', '单位见各字段；这些设置影响该账号的所有请求。', 'Units are shown per field. These settings affect every request using this account.'],
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
      connect_timeout_millis: '建立连接的等待时限，必须小于请求总超时。',
      read_timeout_millis: '收到响应头后等待首段内容，以及后续相邻内容之间允许的最长无数据时间。',
      request_timeout_millis: '从首次发送到完整接收响应的总时限，包含系统允许的重放。',
      max_sse_event_bytes: '单个上游 SSE 事件允许保留的最大字节数；Responses 终止事件可能包含完整响应对象。',
      max_sse_framed_bytes: '单个网络分片可产出的完整 SSE 帧总字节数，必须不小于单事件上限。',
      max_sse_terminal_hold_bytes: '等待 EOF 验证时可暂存的终止事件总字节数，必须不小于单事件上限。',
    })) {
      const field = policy.properties?.[name];
      if (field && typeof field === 'object') field.description = description;
    }
  }
  return result;
}
