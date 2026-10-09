import type { CacheStats } from '../types';

export const cacheStatisticsCopy = {
  'zh-CN': {
    tokens: '缓存读取词元', rate: '缓存命中率', unknown: '未知',
    unavailable: '供应商未提供完整的缓存用量，无法计算命中率。',
    partial: '仅按缓存用量完整的输入计算，部分请求未提供完整的缓存用量。',
    tokenUnavailable: '供应商未报告缓存读取量。',
    tokenPartial: '仅汇总已报告的缓存读取量，部分请求未提供读取量。',
    tokenDefinition: '供应商报告的缓存读取词元总量。',
    noInput: '没有可用于计算命中率的输入词元。',
    definition: '缓存读取词元 ÷ 同一批请求的输入词元，包含缓存读取和写入。',
  },
  en: {
    tokens: 'Cached input tokens', rate: 'Cache hit rate', unknown: 'Unknown',
    unavailable: 'The provider did not report complete cache usage, so the hit rate is unavailable.',
    partial: 'Calculated from inputs with complete cache usage; some requests have incomplete cache data.',
    tokenUnavailable: 'The provider did not report cache reads.',
    tokenPartial: 'Only reported cache reads are included; some requests have no reported read counts.',
    tokenDefinition: 'Total cache-read tokens reported by providers.',
    noInput: 'There are no input tokens available to calculate a hit rate.',
    definition: 'Cached reads divided by input tokens from the same requests, including cache reads and writes.',
  },
};

export function cacheStatisticsPresentation(cache: CacheStats | undefined) {
  const values = cache && [cache.reported_read_tokens, cache.reported_requests, cache.known_read_tokens, cache.known_input_tokens, cache.eligible_requests, cache.unknown_requests];
  const valid = Boolean(values?.every(value => Number.isSafeInteger(value) && value >= 0)
    && cache!.reported_requests <= cache!.eligible_requests + cache!.unknown_requests
    && cache!.eligible_requests <= cache!.reported_requests);
  const known = Boolean(valid
    && cache!.eligible_requests > 0 && cache!.known_read_tokens <= cache!.known_input_tokens);
  const rate = known && cache!.known_input_tokens > 0 && cache!.hit_rate !== null
    && Number.isFinite(cache!.hit_rate) && cache!.hit_rate >= 0 && cache!.hit_rate <= 1
    && Math.abs(cache!.hit_rate - cache!.known_read_tokens / cache!.known_input_tokens) < 1e-12
    ? cache!.hit_rate : null;
  return { tokens: valid && cache!.reported_requests > 0 ? cache!.reported_read_tokens : null, rate,
    note: !known ? 'unavailable' : cache!.known_input_tokens === 0 ? 'noInput'
      : cache!.unknown_requests > 0 ? 'partial' : 'definition' } as const;
}
