import { useCallback, useEffect, useRef, useState } from 'react';
import type { RequestEvent } from '../../types';
import { SessionEventChannel } from '../sessionEventChannel';
import { useRequestEventStream } from './useRequestEventStream';
import { coalesceRequestEvent, RequestRefreshBatch, requestEventCacheCapacity, trimRequestEventCache } from '../traffic/requestRefresh.js';

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
  // Page-local pause tier reported by the Requests consumer. While set, the
  // batch stops publishing (no revision bumps, no rerenders) but keeps
  // coalescing SSE events into its strictly bounded pending map.
  const consumerPause = useRef(false);

  useEffect(() => {
    // Drop the previous scope's protected ids before the new batch adopts
    // them: stale ids would otherwise shield old-scope rows from eviction.
    protectedIds.current = [];
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
      // Publish-side trimming stays silent: pending-side loss already travels
      // with the batch via its overflow flag.
      trimRequestEventCache(published, new Set(protectedIds.current), requestEventCacheCapacity);
      events.current = published;
      if (overflow) setOverflowRevision(value => value + 1);
      setRevision(value => value + 1);
    });
    next.protect(protectedIds.current);
    next.setPaused(paused || !enabled || consumerPause.current);
    batch.current = next;
    return () => { next.dispose(); if (batch.current === next) batch.current = undefined; };
  }, [tenant, token]);

  useEffect(() => { batch.current?.setInterval(intervalMs); }, [intervalMs]);
  useEffect(() => { batch.current?.setPaused(paused || !enabled || consumerPause.current); }, [paused, enabled]);
  // The Sessions surface keeps its own cadence: the request page's pause tier
  // must not hold back session event identities.
  useEffect(() => { sessionEvents.current.setCadence(intervalMs, paused || !enabled); }, [intervalMs, paused, enabled]);
  const protectRequests = useCallback((ids: string[], consumerPaused?: boolean) => {
    protectedIds.current = ids;
    batch.current?.protect(ids);
    // A shrinking protected set tightens the published cache bound right away.
    // Dropped entries request an authoritative first-page reconciliation.
    if (trimRequestEventCache(events.current, new Set(ids), requestEventCacheCapacity)) setOverflowRevision(value => value + 1);
    // Omitted (e.g. scope cleanup) means "keep the current pause tier".
    if (consumerPaused !== undefined) {
      consumerPause.current = consumerPaused;
      batch.current?.setPaused(paused || !enabled || consumerPause.current);
    }
  }, [paused, enabled]);

  const stream = useRequestEventStream({
    token,
    tenant,
    enabled: enabled && !paused,
    disconnectedMessage,
    onEvent: (event) => {
      // Both surfaces publish at the selected cadence, but Sessions keeps its
      // exact identities separate from the bounded request-table cache.
      sessionEvents.current.publish(event);
      batch.current?.enqueue(event);
    },
  });

  return { ...stream, events, sessionEvents: sessionEvents.current, revision, overflowRevision, protectRequests };
}
