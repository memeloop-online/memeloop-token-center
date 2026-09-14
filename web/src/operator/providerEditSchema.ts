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
    reservation_token_bounds: ['模型 Token 预留上限', 'Model token reservation bounds', '按准确模型名称设置保守的 Token 预留值，用于请求预算，不代表模型输出上限。', 'Conservative token reservations per exact model name, used for request budgeting rather than model output limits.'],
    transport_policy: ['超时与重试参数', 'Timeout and retry parameters', '单位见各字段；这些设置影响该账号的所有请求。', 'Units are shown per field. These settings affect every request using this account.'],
  };
  for (const [name, [zhTitle, enTitle, zhHint, enHint]] of Object.entries(copy)) {
    const field = properties.properties?.[name];
    if (field && typeof field === 'object') {
      field.title = zh ? zhTitle : enTitle;
      field.description = zh ? zhHint : enHint;
    }
  }
  return result;
}
