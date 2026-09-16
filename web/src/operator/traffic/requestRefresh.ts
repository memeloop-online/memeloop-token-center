import type { RequestEvent } from '../../types.js';
import { requestViewFromEvent } from './requestTraffic.js';

export const requestRefreshIntervals = [0, 5_000, 30_000, 60_000, 300_000] as const;
export const defaultRequestRefreshInterval = 5_000;
export const requestRefreshPreferenceKey = 'mtc.operator.request-refresh-ms.v1';
export function requestRefreshPreference(value: string | null): number {
  const number = value === null || !value.trim() ? NaN : Number(value);
  return requestRefreshIntervals.some(interval => interval === number) ? number : defaultRequestRefreshInterval;
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
    private capacity = 2_000) {}
  protect(ids: Iterable<string>) { this.protectedIds = new Set(ids); }
  enqueue(event: RequestEvent) {
    if (this.disposed) return;
    this.pending.set(event.request_id, coalesceRequestEvent(this.pending.get(event.request_id), event));
    if (this.pending.size > this.capacity + this.protectedIds.size) {
      for (const id of this.pending.keys()) {
        if (!this.protectedIds.has(id)) { this.pending.delete(id); this.overflow = true; break; }
      }
    }
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
