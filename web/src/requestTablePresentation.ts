import type { RequestView } from './types';

const tokenCount = (value: unknown): value is number => typeof value === 'number' && Number.isSafeInteger(value) && value >= 0;

/** Missing cache telemetry cannot establish an uncached-input count. */
export function nonCachedRequestInput(request: RequestView): number | null {
  if (request.status_code === null) return null;
  const { input_tokens: input, cached_input_tokens: cached, cache_write_tokens: written } = request;
  if (!tokenCount(input) || !tokenCount(cached) || !tokenCount(written) || cached + written > input) return null;
  return input - cached - written;
}

/** End-to-end output throughput, not an inferred model decoding speed. */
export function averageRequestOutputTps(request: RequestView): number | null {
  if (request.status_code === null || !tokenCount(request.output_tokens)
    || request.duration_ms === null || !Number.isFinite(request.duration_ms) || request.duration_ms <= 0) return null;
  const rate = request.output_tokens * 1000 / request.duration_ms;
  return Number.isFinite(rate) ? rate : null;
}

export function requestTableCopy(locale: string) {
  return locale.startsWith('zh') ? {
    total: '总量', input: '非缓存输入', output: '输出', unknown: '未记录', unnamed: '未命名凭据',
    missingCredential: '未记录凭据', averageTps: '平均 TPS',
    tpsHint: '输出 Token ÷ 请求总耗时（秒），包含等待与首字延迟，不代表纯生成速度。',
    tpsMissing: '缺少有效的输出 Token 或请求耗时，无法计算。', tpsRunning: '请求进行中，完成后计算。',
    cacheMissing: '缓存明细缺失或不一致，无法确定非缓存输入；完整已知用量见详情。',
    pendingUsage: '请求进行中，用量与费用尚未结算。',
  } : {
    total: 'Total', input: 'Uncached input', output: 'Output', unknown: 'Not recorded', unnamed: 'Unnamed credential',
    missingCredential: 'Credential not recorded', averageTps: 'Average TPS',
    tpsHint: 'Output tokens ÷ total request seconds, including waiting and first-token latency; not pure decoding speed.',
    tpsMissing: 'Valid output tokens or request duration are missing; the rate cannot be calculated.', tpsRunning: 'Calculated when the request finishes.',
    cacheMissing: 'Cache telemetry is missing or inconsistent, so uncached input is unknown. Known usage remains in details.',
    pendingUsage: 'The request is running; usage and cost have not been settled.',
  };
}

export function requestCredentialLabel(request: RequestView, fallback: string | undefined, locale: string) {
  const copy = requestTableCopy(locale);
  if (request.credential_identity) return request.credential_identity.key_alias?.trim() || copy.unnamed;
  return request.credential_identity === undefined && fallback?.trim() ? fallback.trim() : copy.missingCredential;
}
