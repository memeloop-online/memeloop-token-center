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
  if (requestIsPending(request) || !tokenCount(request.output_tokens)
    || request.duration_ms === null || !Number.isFinite(request.duration_ms) || request.duration_ms <= 0) return null;
  const rate = request.output_tokens * 1000 / request.duration_ms;
  return Number.isFinite(rate) ? rate : null;
}

export function requestCredentialLabel(request: RequestView, fallback: string | undefined): { label: string } | { key: 'request.unnamedCredential' | 'request.missingCredential' } {
  if (request.credential_identity) {
    const label = request.credential_identity.key_alias?.trim();
    return label ? { label } : { key: 'request.unnamedCredential' };
  }
  return request.credential_identity === undefined && fallback?.trim()
    ? { label: fallback.trim() } : { key: 'request.missingCredential' };
}
