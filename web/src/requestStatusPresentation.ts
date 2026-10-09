import type { RequestView } from './types.js';

export type RequestOutcome = 'running' | 'delivering' | 'completed' | 'cancelled' | 'interrupted' | 'failed' | 'unknown';

const recordedFailureCopy: Record<string, [string, string]> = {
  upstream_eof_without_terminal: ['上游响应在结束事件前中断', 'Upstream response ended before a terminal event'],
  upstream_stream_read_error: ['读取上游响应时连接中断', 'The upstream response stream was interrupted'],
  upstream_stream: ['上游响应流中断', 'The upstream response stream was interrupted'],
  upstream_incomplete_response: ['上游响应未完整结束', 'The upstream response did not finish'],
  upstream_response_terminal_conflict: ['上游响应包含冲突的结束信息', 'The upstream response contained conflicting terminal information'],
  upstream_transport_timeout: ['等待上游响应超时', 'The upstream request timed out'],
  upstream_transport_outer_deadline: ['等待上游响应超过请求时限', 'The upstream request exceeded its overall deadline'],
  upstream_transport_connection_reset: ['上游连接被重置', 'The upstream connection was reset'],
  upstream_transport_http2_reset: ['上游 HTTP/2 流被重置', 'The upstream HTTP/2 stream was reset'],
  upstream_transport_http2_goaway: ['上游 HTTP/2 连接收到 GOAWAY', 'The upstream HTTP/2 connection received GOAWAY'],
  upstream_http2_reset: ['上游 HTTP/2 流被重置', 'The upstream HTTP/2 stream was reset'],
  upstream_http2_goaway: ['上游 HTTP/2 连接收到 GOAWAY', 'The upstream HTTP/2 connection received GOAWAY'],
  upstream_transport_body: ['发送上游请求内容失败', 'Sending the upstream request body failed'],
  upstream_transport_decode: ['解析上游响应失败', 'Decoding the upstream response failed'],
  upstream_transport_request: ['发送上游请求失败', 'Sending the upstream request failed'],
  upstream_transport_other: ['上游网络请求失败', 'The upstream network request failed'],
  upstream_connection: ['上游连接失败', 'The upstream connection failed'],
  upstream_read_timeout: ['读取上游响应超时', 'Reading the upstream response timed out'],
  upstream_request_timeout: ['等待上游请求响应超时', 'The upstream request timed out'],
};

const supplierFailureCopy: Record<string, [string, string]> = {
  no_active_plan: ['当前上游账号没有可用套餐，需在提供商处开通或更换账号。', 'The upstream account has no active plan. Activate a plan with the provider or use another account.'],
  insufficient_quota: ['当前上游账号额度不足，需在提供商处补充额度或更换账号。', 'The upstream account has insufficient quota. Add quota with the provider or use another account.'],
  authentication_invalid: ['上游账号凭据无效，需检查或更换该账号的凭据。', 'The upstream account credentials are invalid. Check or replace the account credentials.'],
  authentication_expired: ['上游账号凭据已过期，需更新该账号的凭据。', 'The upstream account credentials have expired. Update the account credentials.'],
  rate_limited: ['上游服务已限制请求频率，请稍后重试。', 'The upstream service has limited the request rate. Try again later.'],
  model_unavailable: ['上游账号无法使用所请求的模型，需检查模型权限或更换模型。', 'The requested model is unavailable to the upstream account. Check model access or use another model.'],
};

const supplierVocabulary: Record<string, { base: string; codes: string[]; messages: string[] }> = {
  no_active_plan: { base: '当前账号没有可用套餐', codes: ['no_active_plan', 'NoAvailablePlan', 'NoActivePlan', 'NoPlan', '402'], messages: ['当前账号没有可用套餐', 'No active plan', 'No available plan'] },
  insufficient_quota: { base: 'The upstream account has insufficient quota', codes: ['insufficient_quota'], messages: ['Quota exhausted', 'Quota exhausted for this account', 'Insufficient quota'] },
  authentication_invalid: { base: 'The upstream authentication credentials are invalid', codes: ['authentication_invalid', 'invalid_api_key'], messages: ['Invalid API key', 'Invalid authentication credentials'] },
  authentication_expired: { base: 'The upstream authentication credentials have expired', codes: ['authentication_expired', 'api_key_expired', 'token_expired'], messages: ['API key expired', 'Authentication token expired'] },
  rate_limited: { base: 'The upstream request rate limit was exceeded', codes: ['rate_limited', 'rate_limit_exceeded'], messages: ['Rate limit exceeded', 'Too many requests'] },
  model_unavailable: { base: 'The requested upstream model is unavailable', codes: ['model_unavailable', 'model_not_found', 'unsupported_model', '400'], messages: ['Model unavailable', 'Unsupported model', 'Model not found'] },
};

export function requestSupplierDetail(request: RequestView, locale: 'zh-CN' | 'en'): string | null {
  const supplier = request.supplier_error;
  if (!supplier || !Object.hasOwn(supplierVocabulary, supplier.code)) return null;
  const vocabulary = supplierVocabulary[supplier.code];
  const match = /^(.*?)(?:; provider code: ([^;]+))?(?:; provider message: ([^;]+))?$/.exec(supplier.message);
  if (!match || match[0] !== supplier.message || match[1] !== vocabulary.base) return null;
  const [, , code, message] = match;
  if ((!code && !message) || (code && !vocabulary.codes.includes(code)) || (message && !vocabulary.messages.includes(message))) return null;
  return [code && `${locale === 'zh-CN' ? '供应商错误码' : 'Provider error code'}: ${code}`,
    message && `${locale === 'zh-CN' ? '供应商说明' : 'Provider message'}: ${message}`].filter(Boolean).join(' · ');
}

function recordedCause(errorCode: string, locale: 'zh-CN' | 'en'): string | null {
  const cause = recordedFailureCopy[errorCode];
  return cause ? cause[locale === 'zh-CN' ? 0 : 1] : null;
}

export function requestFailureCause(request: RequestView, locale: 'zh-CN' | 'en'): string | null {
  const terminal = request.terminal_cause_code ? recordedCause(request.terminal_cause_code, locale) : null;
  const transport = terminal ?? (request.error_code ? recordedCause(request.error_code, locale) : null);
  if (transport) return transport;
  if (!['failed', 'interrupted'].includes(requestOutcome(request))) return null;
  const supplier = request.supplier_error?.code;
  const copy = supplier && Object.hasOwn(supplierFailureCopy, supplier) ? supplierFailureCopy[supplier] : null;
  return copy ? copy[locale === 'zh-CN' ? 0 : 1] : null;
}

export function requestErrorCopy(errorCode: string, locale: 'zh-CN' | 'en') {
  const cause = recordedCause(errorCode, locale);
  if (cause) return cause;
  if (errorCode.startsWith('upstream_')) return locale === 'zh-CN' ? '上游服务返回错误' : 'The upstream service returned an error';
  if (errorCode.startsWith('downstream_')) return locale === 'zh-CN' ? '响应交付中断' : 'Response delivery was interrupted';
  if (errorCode.startsWith('client_')) return locale === 'zh-CN' ? '客户端未完成请求' : 'The client did not complete the request';
  return locale === 'zh-CN' ? '请求失败' : 'The request failed';
}

/** Recorded terminal evidence, never an inference from initial HTTP headers. */
export function requestOutcome(request: RequestView): RequestOutcome {
  if (request.status_code === null) return request.error_code === 'delivery_started' ? 'delivering' : 'running';
  if (request.error_code === 'client_cancelled' || request.error_code === 'downstream_disconnected') return 'cancelled';
  if (['upstream_incomplete_response', 'upstream_stream', 'downstream_backpressure'].includes(request.error_code ?? '')) return 'interrupted';
  if (request.error_code || request.status_code >= 400) return 'failed';
  if (request.status_code < 200) return 'unknown';
  return typeof request.completed_at === 'number' && Number.isFinite(request.completed_at) && request.completed_at >= request.created_at
    ? 'completed' : 'unknown';
}

export function requestStatusCopy(request: RequestView, locale: 'zh-CN' | 'en') {
  const outcome = requestOutcome(request);
  const copy = locale === 'zh-CN' ? {
    running: ['运行中', '尚未记录完成状态，用量和费用仍待结算。'],
    delivering: ['交付中', '已开始发送响应，交付完成状态待记录。'],
    completed: ['已完成', '服务端已记录正常结束；不代表客户端已确认接收全部内容。'],
    cancelled: ['客户端断开', '客户端连接已断开，响应未正常交付完成。'],
    interrupted: ['响应中断', '响应未正常结束；已记录用量不代表响应完整交付。'],
    failed: ['失败', ''],
    unknown: ['记录待补充', '此历史记录缺少完成时间，无法确认完整交付。'],
  } : {
    running: ['Running', 'No terminal outcome is recorded yet. Usage and cost await settlement.'],
    delivering: ['Delivering', 'Response delivery has started, but its terminal outcome is not recorded yet.'],
    completed: ['Completed', 'The server recorded a normal finish; this is not client acknowledgement of the complete response.'],
    cancelled: ['Client disconnected', 'The client connection closed before normal response delivery completed.'],
    interrupted: ['Interrupted', 'The response did not finish normally. Recorded usage does not establish complete delivery.'],
    failed: ['Failed', ''],
    unknown: ['Record incomplete', 'Completion time is missing; complete delivery is not confirmed.'],
  };
  const [label, explanation] = copy[outcome];
  const code = request.status_code === null ? '' : `${locale === 'zh-CN' ? '记录状态码' : 'Recorded status'}: ${request.status_code}`;
  const cause = ['failed', 'interrupted'].includes(outcome) ? requestFailureCause(request, locale) : null;
  const failureDetail = ['failed', 'interrupted'].includes(outcome)
    ? `${locale === 'zh-CN' ? '已记录原因' : 'Recorded cause'}: ${cause ?? (locale === 'zh-CN' ? '未知（未记录具体原因）' : 'Unknown (no specific cause recorded)')}` : '';
  return { outcome, label, tone: outcome === 'completed' ? 'ok' : ['cancelled', 'interrupted', 'failed'].includes(outcome) ? 'bad' : outcome === 'unknown' ? 'unknown' : 'pending',
    cause: failureDetail,
    supplierDetail: requestSupplierDetail(request, locale),
    hint: ['failed', 'interrupted'].includes(outcome) ? [failureDetail, requestSupplierDetail(request, locale)].filter(Boolean).join(' · ') : [explanation, code].filter(Boolean).join(' · ') };
}
