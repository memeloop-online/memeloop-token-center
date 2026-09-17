import type { RequestEvent, RequestView } from '../../types.js';
import { mergeLiveRequestEvents, requestViewFromEvent } from './requestTraffic.js';

export const requestRefreshIntervals = [0, 5_000, 30_000, 60_000, 300_000] as const;
export const defaultRequestRefreshInterval = 5_000;
export const requestRefreshPreferenceKey = 'mtc.operator.request-refresh-ms.v1';
export const requestEventCacheCapacity = 2_000;
export function requestRefreshPreference(value: string | null): number {
  const number = value === null || !value.trim() ? NaN : Number(value);
  return requestRefreshIntervals.some(interval => interval === number) ? number : defaultRequestRefreshInterval;
}

/** Automatic traffic never grows the rendered window; explicit history pages still do. */
export function mergeBatchedRequestPage(snapshot: RequestView[], events: Map<string, RequestEvent>, hasOlder: boolean, historyIds: ReadonlySet<string> = new Set()) {
  if (historyIds.size) {
    // A history cursor describes a contiguous retained window. Prepending and
    // evicting rows above it would leave gaps unreachable through Load older.
    const visible = new Set(snapshot.map(request => request.request_id));
    return { requests: mergeLiveRequestEvents(snapshot, new Map([...events].filter(([id]) => visible.has(id))), true), hasOlder };
  }
  const merged = mergeLiveRequestEvents(snapshot, events, true);
  const requests = merged.slice(0, 100);
  return { requests, hasOlder: hasOlder || merged.length > requests.length };
}

/** Merge partial archive/projection updates without losing a terminal already observed. */
export function coalesceRequestEvent(previous: RequestEvent | undefined, event: RequestEvent): RequestEvent {
  if (!previous) return event;
  if (event.event_at < previous.event_at || (event.event_at === previous.event_at && event.event_id <= previous.event_id)) return previous;
  const prior = requestViewFromEvent(previous);
  const effective = prior?.status_code != null && event.status_code == null
    ? { ...event, status_code: prior.status_code, duration_ms: prior.duration_ms, error_code: prior.error_code,
      input_tokens: prior.input_tokens, output_tokens: prior.output_tokens, cost: prior.cost } : event;
  return { ...event, ...requestViewFromEvent(effective, prior) };
}

export interface RefreshClock {
  schedule: (callback: () => void, delay: number) => number;
  cancel: (timer: number) => void;
}

/** Evict oldest unprotected entries until the cache fits its bound. Returns whether anything was dropped. */
export function trimRequestEventCache<V>(cache: Map<string, V>, protectedIds: ReadonlySet<string>, capacity = requestEventCacheCapacity): boolean {
  let evicted = false;
  for (const id of cache.keys()) {
    if (cache.size <= capacity + protectedIds.size) break;
    if (protectedIds.has(id)) continue;
    cache.delete(id);
    evicted = true;
  }
  return evicted;
}

/** One timer per batch, never a sliding debounce. Real time still yields between batches. */
export class RequestRefreshBatch {
  private timer?: number;
  private pending = new Map<string, RequestEvent>();
  private protectedIds = new Set<string>();
  private overflow = false;
  private paused = false;
  private disposed = false;
  constructor(private interval: number, private clock: RefreshClock,
    private publish: (events: Map<string, RequestEvent>, overflow: boolean) => void,
    private capacity = requestEventCacheCapacity) {}
  protect(ids: Iterable<string>) {
    this.protectedIds = new Set(ids);
    // A shrinking protected set tightens the bound immediately; dropped
    // pending events must surface as overflow on the next publish.
    if (trimRequestEventCache(this.pending, this.protectedIds, this.capacity)) this.overflow = true;
  }
  enqueue(event: RequestEvent) {
    if (this.disposed) return;
    this.pending.set(event.request_id, coalesceRequestEvent(this.pending.get(event.request_id), event));
    if (trimRequestEventCache(this.pending, this.protectedIds, this.capacity)) this.overflow = true;
    this.schedule();
  }
  setInterval(interval: number) { this.interval = interval; this.cancel(); this.schedule(); }
  setPaused(paused: boolean) { this.paused = paused; this.cancel(); this.schedule(); }
  private cancel() { if (this.timer !== undefined) this.clock.cancel(this.timer); this.timer = undefined; }
  private schedule() {
    if (this.disposed || this.paused || this.timer !== undefined || !this.pending.size) return;
    this.timer = this.clock.schedule(() => {
      this.timer = undefined;
      const events = this.pending; const overflow = this.overflow;
      this.pending = new Map(); this.overflow = false;
      this.publish(events, overflow);
    }, Math.max(16, this.interval));
  }
  dispose() { this.disposed = true; this.cancel(); this.pending.clear(); this.protectedIds.clear(); }
}
