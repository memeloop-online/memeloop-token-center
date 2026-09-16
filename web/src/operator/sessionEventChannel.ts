import type { RequestEvent } from '../types.js';
import { enqueueSessionEventIdentity } from './sessionRefresh.js';

/** Prompt, route-local session invalidation without waking the traffic route. */
export class SessionEventChannel {
  readonly eventKeyIds = { current: new Set<string>() };
  private listeners = new Set<() => void>();
  private revision = 0;
  subscribe = (listener: () => void) => { this.listeners.add(listener); return () => this.listeners.delete(listener); };
  snapshot = () => this.revision;
  publish(event: RequestEvent) {
    // The Sessions route is the only consumer. Do not retain an unbounded
    // side queue or rerender Requests when that route is not mounted.
    if (this.listeners.size === 0) return;
    enqueueSessionEventIdentity(this.eventKeyIds.current, event);
    this.revision += 1;
    for (const listener of this.listeners) listener();
  }
  clear() { this.eventKeyIds.current.clear(); }
}
