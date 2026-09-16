import type { RequestView } from './types.js';

const tokenCount = (value: unknown): value is number => typeof value === 'number' && Number.isSafeInteger(value) && value >= 0;

/** Nonnegative finite safe milliseconds; fractional, negative or unsafe timing telemetry is unusable. */
const timingMs = (value: unknown): value is number => typeof value === 'number' && Number.isSafeInteger(value) && value >= 0;

/** Live status is written atomically with settlement; imported history may lack completed_at. */
export function requestIsPending(request: RequestView): boolean {
  return request.status_code === null;
}

/** Recorded terminal failure evidence, including client disconnects recorded as 499. */
export function requestFailed(request: RequestView): boolean {
  return !requestIsPending(request) && (Boolean(request.error_code) || (request.status_code ?? 0) >= 400);
}

/**
 * Pending usage is never actual: it is not yet settled and must not count toward
 * aggregate traffic. Failed requests only have actual usage when the provider
 * reported it; estimates, settlement ceilings and unobserved or unrecorded counts
 * stay in details instead of being presented as actual consumption.
 */
export function requestUsageIsActual(request: RequestView): boolean {
  if (requestIsPending(request)) return false;
  return request.usage_basis !== 'provider_estimated'
    && request.usage_basis !== 'contract_ceiling'
    && request.usage_basis !== 'not_observed'
    && (!requestFailed(request) || request.usage_basis === 'provider_reported');
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
  if (request.usage_basis !== 'provider_reported' || request.compaction === true || requestIsPending(request) || !tokenCount(request.output_tokens)
    || !timingMs(request.duration_ms) || request.duration_ms <= 0) return null;
  const rate = request.output_tokens * 1000 / request.duration_ms;
  return Number.isFinite(rate) ? rate : null;
}

/** Gateway-observed output rate over the recorded first-output-to-terminal interval; may include buffered delivery, never a model decoding measurement. */
export function generationRequestOutputTps(request: RequestView): number | null {
  const first = request.first_output_ms;
  const generation = request.generation_duration_ms;
  if (request.usage_basis !== 'provider_reported' || request.compaction === true || requestIsPending(request) || !tokenCount(request.output_tokens)
    || !timingMs(first) || !timingMs(generation) || generation <= 0) return null;
  if (!timingMs(request.duration_ms) || request.duration_ms < first + generation) return null;
  const rate = request.output_tokens * 1000 / generation;
  return Number.isFinite(rate) ? rate : null;
}

export function requestUsageCopy(request: RequestView, locale: string) {
  const zh = locale === 'zh-CN';
  switch (request.usage_basis) {
    case 'provider_reported': return { label: zh ? '上游报告用量' : 'Provider-reported usage', hint: zh ? '词元数量由上游报告；速率使用网关记录的时间。' : 'Token counts reported by the provider; throughput uses gateway-recorded timing.' };
    case 'provider_estimated': return { label: zh ? '估算用量' : 'Estimated usage', hint: zh ? '词元数量为估算值，实际输出速率未知。' : 'Token counts are estimated; actual output throughput is unknown.' };
    case 'contract_ceiling': return { label: zh ? '结算上限' : 'Settlement ceiling', hint: zh ? '词元数量为保守结算上限；实际用量与速率请以供应商记录为准。' : 'Token counts are conservative settlement ceilings; use provider records for actual usage and throughput.' };
    case 'not_observed': return { label: zh ? '未观测用量' : 'Usage not observed', hint: zh ? '未观测到实际词元用量；记录计数不能作为实际输出或速率。' : 'Actual token usage was not observed; recorded counts cannot represent actual output or throughput.' };
    default: return { label: zh ? '用量来源未记录' : 'Usage source not recorded', hint: zh ? '历史词元计数来源未记录，不能确认实际输出速率。' : 'The source of historical token counts was not recorded; actual output throughput cannot be confirmed.' };
  }
}

export function requestCostCopy(request: RequestView, locale: string) {
  const zh = locale === 'zh-CN';
  /** Recorded local ledger amounts stay visible in details, explicitly labelled as local settlement rather than supplier usage. */
  const ledgerLabel = zh ? '本地账本金额' : 'Local ledger amount';
  if (requestIsPending(request)) return {
    unknown: true,
    ledgerLabel,
    label: zh ? '待结算' : 'Awaiting settlement',
    hint: zh ? '请求仍在进行中，费用尚未结算。' : 'The request is still in progress; cost has not been settled.',
  };
  if (requestFailed(request) && request.usage_basis === 'not_observed') return {
    unknown: false,
    ledgerLabel,
    label: zh ? '未观测用量' : 'Usage not observed',
    hint: zh
      ? '这是失败请求，未观测到供应商实际用量；本地计费默认按 0 显示。'
      : 'This request failed without observed supplier usage; local billing defaults to 0.',
  };
  if (!requestUsageIsActual(request)) return {
    unknown: true, ledgerLabel,
    label: zh ? '需核对' : 'Review required',
    hint: zh ? '实际用量尚未确认；已记账金额可用于核对。' : 'Actual usage is pending confirmation; the recorded ledger amount is available for review.',
  };
  if (request.usage_basis === 'contract_ceiling') return {
    unknown: false,
    ledgerLabel,
    label: '',
    hint: zh
      ? '历史本地结算采用保守合同上限；供应商实际用量与发票请以供应商记录为准。'
      : 'Historical local settlement used a conservative contract ceiling; use provider records for actual usage and invoice details.',
  };
  return {
    unknown: false,
    ledgerLabel,
    label: '',
    hint: zh
      ? '本地已结算金额；供应商实际用量请以供应商记录为准。'
      : 'Locally settled amount; consult provider records for actual usage.',
  };
}

/** Failed requests without supplier evidence display the policy amount (zero) even before a historical rebate is posted. */
export function requestDisplayedCost(request: RequestView): string {
  return requestFailed(request) && request.usage_basis === 'not_observed' ? '0' : request.cost;
}

export function requestCredentialLabel(request: RequestView, fallback: string | undefined): { label: string } | { key: 'request.unnamedCredential' | 'request.missingCredential' } {
  if (request.credential_identity) {
    const label = request.credential_identity.key_alias?.trim();
    return label ? { label } : { key: 'request.unnamedCredential' };
  }
  return request.credential_identity === undefined && fallback?.trim()
    ? { label: fallback.trim() } : { key: 'request.missingCredential' };
}
