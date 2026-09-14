import type { RequestView } from './types.js';

const tokenCount = (value: unknown): value is number => typeof value === 'number' && Number.isSafeInteger(value) && value >= 0;

/** Live status is written atomically with settlement; imported history may lack completed_at. */
export function requestIsPending(request: RequestView): boolean {
  return request.status_code === null;
}

/** Stored input includes both cache components, including normalized Anthropic usage. */
export function nonCachedRequestInput(request: RequestView): number | null {
  if (requestIsPending(request)) return null;
  const { input_tokens: input, cached_input_tokens: cached, cache_write_tokens: written } = request;
  if (!tokenCount(input) || !tokenCount(cached) || !tokenCount(written) || cached + written > input) return null;
  return input - cached - written;
}

/** End-to-end output throughput, not an inferred model decoding speed. */
export function averageRequestOutputTps(request: RequestView): number | null {
  if (request.usage_basis !== 'provider_reported' || requestIsPending(request) || !tokenCount(request.output_tokens)
    || request.duration_ms === null || !Number.isFinite(request.duration_ms) || request.duration_ms <= 0) return null;
  const rate = request.output_tokens * 1000 / request.duration_ms;
  return Number.isFinite(rate) ? rate : null;
}

/** Provider-reported output over the recorded output interval, excluding initial waiting. */
export function generationRequestOutputTps(request: RequestView): number | null {
  const duration = request.generation_duration_ms;
  if (request.usage_basis !== 'provider_reported' || requestIsPending(request) || !tokenCount(request.output_tokens)
    || !tokenCount(request.first_output_ms) || typeof duration !== 'number' || !Number.isFinite(duration) || duration <= 0) return null;
  const rate = request.output_tokens * 1000 / duration;
  return Number.isFinite(rate) ? rate : null;
}

export function requestUsageCopy(request: RequestView, locale: string) {
  const zh = locale === 'zh-CN';
  switch (request.usage_basis) {
    case 'provider_reported': return { label: zh ? '上游报告用量' : 'Provider-reported usage', hint: zh ? '词元数量由上游报告；速率使用网关记录的时间。' : 'Token counts reported by the provider; throughput uses gateway-recorded timing.' };
    case 'provider_estimated': return { label: zh ? '估算用量' : 'Estimated usage', hint: zh ? '词元数量为估算值，实际输出速率未知。' : 'Token counts are estimated; actual output throughput is unknown.' };
    case 'contract_ceiling': return { label: zh ? '结算上限' : 'Settlement ceiling', hint: zh ? '词元数量为保守结算上限，不是实际生成量；实际用量与速率未知。' : 'Token counts are conservative settlement ceilings, not actual generated output; actual usage and throughput are unknown.' };
    case 'not_observed': return { label: zh ? '未观测用量' : 'Usage not observed', hint: zh ? '未观测到实际词元用量；记录计数不能作为实际输出或速率。' : 'Actual token usage was not observed; recorded counts cannot represent actual output or throughput.' };
    default: return { label: zh ? '用量来源未记录' : 'Usage source not recorded', hint: zh ? '历史词元计数来源未记录，不能确认实际输出速率。' : 'The source of historical token counts was not recorded; actual output throughput cannot be confirmed.' };
  }
}

export function requestCredentialLabel(request: RequestView, fallback: string | undefined): { label: string } | { key: 'request.unnamedCredential' | 'request.missingCredential' } {
  if (request.credential_identity) {
    const label = request.credential_identity.key_alias?.trim();
    return label ? { label } : { key: 'request.unnamedCredential' };
  }
  return request.credential_identity === undefined && fallback?.trim()
    ? { label: fallback.trim() } : { key: 'request.missingCredential' };
}
