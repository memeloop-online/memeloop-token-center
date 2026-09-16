import { useCallback, useEffect, useRef, useState } from 'react';
import type { RequestEvent } from '../../types.js';
import { enqueueSessionEventIdentity } from '../sessionRefresh.js';
import { useRequestEventStream } from './useRequestEventStream.js';
import { coalesceRequestEvent, RequestRefreshBatch } from '../traffic/requestRefresh.js';

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

export function useOperatorRequestStream({ token, tenant, enabled, disconnectedMessage, intervalMs = 5000, paused = false }: {
  token: string;
  tenant: string;
  enabled: boolean;
  disconnectedMessage: string;
  intervalMs?: number;
  paused?: boolean;
}) {
  const events = useRef(new Map<string, RequestEvent>());
  const sessionEvents = useRef(new SessionEventChannel());
  const [revision, setRevision] = useState(0);
  const [overflowRevision, setOverflowRevision] = useState(0);
  const batch = useRef<RequestRefreshBatch | undefined>(undefined);
  const protectedIds = useRef<string[]>([]);

  useEffect(() => {
    events.current.clear();
    sessionEvents.current.clear();
    setRevision((value) => value + 1);
    const next = new RequestRefreshBatch(intervalMs, {
      schedule: (callback, delay) => window.setTimeout(callback, delay), cancel: timer => window.clearTimeout(timer),
    }, (values, overflow) => {
      const published = new Map(events.current);
      for (const [id, event] of values) {
        const next = coalesceRequestEvent(published.get(id), event);
        published.delete(id); published.set(id, next);
      }
      const protectedSet = new Set(protectedIds.current);
      for (const id of published.keys()) {
        if (published.size <= 2_000 + protectedSet.size) break;
        if (!protectedSet.has(id)) published.delete(id);
      }
      events.current = published;
      if (overflow) setOverflowRevision(value => value + 1);
      setRevision(value => value + 1);
    });
    next.protect(protectedIds.current);
    next.setPaused(paused || !enabled);
    batch.current = next;
    return () => { next.dispose(); if (batch.current === next) batch.current = undefined; };
  }, [tenant, token]);

  useEffect(() => { batch.current?.setInterval(intervalMs); }, [intervalMs]);
  useEffect(() => { batch.current?.setPaused(paused || !enabled); }, [paused, enabled]);
  const protectRequests = useCallback((ids: string[]) => { protectedIds.current = ids; batch.current?.protect(ids); }, []);

  const stream = useRequestEventStream({
    token,
    tenant,
    enabled: enabled && !paused,
    disconnectedMessage,
    onEvent: (event) => {
      // Session state filters must not inherit the operator traffic page's
      // user-selected rendering cadence. A terminal event has to invalidate
      // an active-session row promptly, while request-table rendering remains
      // coalesced in the batch below.
      sessionEvents.current.publish(event);
      batch.current?.enqueue(event);
    },
  });

  return { ...stream, events, sessionEvents: sessionEvents.current, revision, overflowRevision, protectRequests };
}
