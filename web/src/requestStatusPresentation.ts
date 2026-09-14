import type { RequestView } from './types.js';

export type RequestOutcome = 'running' | 'delivering' | 'completed' | 'cancelled' | 'interrupted' | 'failed' | 'unknown';

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
    running: ['运行中', '尚未记录终态，用量和费用仍待结算。'],
    delivering: ['交付中', '已开始发送响应，尚未记录交付终态。'],
    completed: ['已完成', '服务端已记录正常结束；不代表客户端已确认接收全部内容。'],
    cancelled: ['客户端断开', '客户端连接已断开，响应未正常交付完成。'],
    interrupted: ['响应中断', '响应未正常结束；已记录用量不代表响应完整交付。'],
    failed: ['失败', '服务端记录了请求失败，请查看错误详情。'],
    unknown: ['终态未知', '历史记录仅保留状态码，缺少完成时间；无法确认完整交付。'],
  } : {
    running: ['Running', 'No terminal outcome is recorded yet. Usage and cost await settlement.'],
    delivering: ['Delivering', 'Response delivery has started, but its terminal outcome is not recorded yet.'],
    completed: ['Completed', 'The server recorded a normal finish; this is not client acknowledgement of the complete response.'],
    cancelled: ['Client disconnected', 'The client connection closed before normal response delivery completed.'],
    interrupted: ['Interrupted', 'The response did not finish normally. Recorded usage does not establish complete delivery.'],
    failed: ['Failed', 'The server recorded a request failure. See the error details.'],
    unknown: ['Outcome unknown', 'This historical record retains a status code but no completion time; complete delivery cannot be confirmed.'],
  };
  const [label, explanation] = copy[outcome];
  const code = request.status_code === null ? '' : `${locale === 'zh-CN' ? '记录状态码' : 'Recorded status'}: ${request.status_code}`;
  return { outcome, label, tone: outcome === 'completed' ? 'ok' : ['cancelled', 'interrupted', 'failed'].includes(outcome) ? 'bad' : outcome === 'unknown' ? 'unknown' : 'pending',
    hint: [explanation, code, request.error_code].filter(Boolean).join(' · ') };
}
