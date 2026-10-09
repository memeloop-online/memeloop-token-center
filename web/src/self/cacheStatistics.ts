import type { CacheStats } from '../types';

export const cacheStatisticsCopy = {
  'zh-CN': {
    tokens: '缓存读取词元', rate: '缓存命中率', unknown: '未知',
    unavailable: '供应商未提供完整的缓存用量，无法计算命中率。',
    partial: '仅按已报告缓存用量的输入计算，部分请求未提供缓存数据。',
    noInput: '没有可用于计算命中率的输入词元。',
    definition: '缓存读取词元 ÷ 同一批请求的输入词元，包含缓存读取和写入。',
  },
  en: {
    tokens: 'Cached input tokens', rate: 'Cache hit rate', unknown: 'Unknown',
    unavailable: 'The provider did not report complete cache usage, so the hit rate is unavailable.',
    partial: 'Calculated from inputs with reported cache usage; some requests have no cache data.',
    noInput: 'There are no input tokens available to calculate a hit rate.',
    definition: 'Cached reads divided by input tokens from the same requests, including cache reads and writes.',
  },
};

export function cacheStatisticsPresentation(cache: CacheStats | undefined) {
  const values = cache && [cache.known_read_tokens, cache.known_input_tokens, cache.eligible_requests, cache.unknown_requests];
  const known = Boolean(values?.every(value => Number.isSafeInteger(value) && value >= 0)
    && cache!.eligible_requests > 0 && cache!.known_read_tokens <= cache!.known_input_tokens);
  const rate = known && cache!.known_input_tokens > 0 && cache!.hit_rate !== null
    && Number.isFinite(cache!.hit_rate) && cache!.hit_rate >= 0 && cache!.hit_rate <= 1
    && Math.abs(cache!.hit_rate - cache!.known_read_tokens / cache!.known_input_tokens) < 1e-12
    ? cache!.hit_rate : null;
  return { tokens: known ? cache!.known_read_tokens : null, rate,
    note: !known ? 'unavailable' : cache!.known_input_tokens === 0 ? 'noInput'
      : cache!.unknown_requests > 0 ? 'partial' : 'definition' } as const;
}
