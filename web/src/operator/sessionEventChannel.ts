import type { RequestEvent } from '../types.js';
import { enqueueSessionEventIdentity, sessionRefreshDelayMs } from './sessionRefresh.js';

const SESSION_EVENT_QUEUE_LIMIT = 2_000;

export interface SessionEventClock {
  schedule: (callback: () => void, delay: number) => number;
  cancel: (timer: number) => void;
}

/** Prompt, route-local session invalidation without waking the traffic route. */
export class SessionEventChannel {
  readonly eventKeyIds = { current: new Set<string>() };
  readonly overflowed = { current: false };
  private listeners = new Set<() => void>();
  private revision = 0;
  private intervalMs = 5_000;
  private paused = false;
  private timer?: number;
  constructor(private clock: SessionEventClock = {
    schedule: (callback, delay) => window.setTimeout(callback, delay),
    cancel: timer => window.clearTimeout(timer),
  }) {}
  subscribe = (listener: () => void) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
      if (this.listeners.size === 0) this.clear();
    };
  };
  snapshot = () => this.revision;
  setCadence(intervalMs: number, paused: boolean) {
    this.intervalMs = intervalMs;
    this.paused = paused;
    if (this.timer !== undefined) this.clock.cancel(this.timer);
    this.timer = undefined;
    this.schedule();
  }
  private schedule() {
    if (this.paused || this.timer !== undefined || this.listeners.size === 0 || this.eventKeyIds.current.size === 0) return;
    this.timer = this.clock.schedule(() => {
      this.timer = undefined;
      if (this.paused || this.listeners.size === 0 || this.eventKeyIds.current.size === 0) return;
      this.revision += 1;
      for (const listener of this.listeners) listener();
    }, sessionRefreshDelayMs(this.intervalMs));
  }
  publish(event: RequestEvent) {
    // The Sessions route is the only consumer. Do not retain an unbounded
    // side queue or rerender Requests when that route is not mounted.
    if (this.listeners.size === 0) return;
    enqueueSessionEventIdentity(this.eventKeyIds.current, event);
    while (this.eventKeyIds.current.size > SESSION_EVENT_QUEUE_LIMIT) {
      const oldest = this.eventKeyIds.current.values().next().value;
      if (oldest === undefined) break;
      this.eventKeyIds.current.delete(oldest);
      this.overflowed.current = true;
    }
    this.schedule();
  }
  clear() {
    if (this.timer !== undefined) this.clock.cancel(this.timer);
    this.timer = undefined;
    this.eventKeyIds.current.clear();
    this.overflowed.current = false;
  }
}
