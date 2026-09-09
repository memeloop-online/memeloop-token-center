import type { TypedFilterAst } from '../types.js';

export interface RequestDrilldown {
  ast: TypedFilterAst;
  revision: number;
}

/**
 * A chart bucket has only an exact UTC start and the server-selected
 * granularity. Keep the request drilldown limited to those recorded facts;
 * success, failure, latency, and cost series all use this same time window.
 */
export function requestDrilldownForOverviewBucket(
  bucketStart: number,
  granularity: 'hour' | 'day',
): TypedFilterAst | undefined {
  if (!Number.isSafeInteger(bucketStart) || bucketStart < 0) return undefined;
  const duration = granularity === 'hour' ? 3_600_000 : 86_400_000;
  const bucketEnd = bucketStart + duration - 1;
  if (!Number.isSafeInteger(bucketEnd)) return undefined;
  return {
    logical_operator: 'and',
    conditions: [{
      field: 'created_at',
      operator: 'between',
      value: { type: 'timestamp', value: bucketStart },
      upper: { type: 'timestamp', value: bucketEnd },
    }],
  };
}
