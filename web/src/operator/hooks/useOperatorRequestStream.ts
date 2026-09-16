import { useCallback, useEffect, useRef, useState } from 'react';
import type { RequestEvent } from '../../types';
import { enqueueSessionEventIdentity } from '../sessionRefresh';
import { useRequestEventStream } from './useRequestEventStream';
import { coalesceRequestEvent, RequestRefreshBatch } from '../traffic/requestRefresh.js';

export function useOperatorRequestStream({ token, tenant, enabled, disconnectedMessage, intervalMs = 5000, paused = false }: {
  token: string;
  tenant: string;
  enabled: boolean;
  disconnectedMessage: string;
  intervalMs?: number;
  paused?: boolean;
}) {
  const events = useRef(new Map<string, RequestEvent>());
  const sessionEventKeyIds = useRef(new Set<string>());
  const [revision, setRevision] = useState(0);
  const [overflowRevision, setOverflowRevision] = useState(0);
  const batch = useRef<RequestRefreshBatch | undefined>(undefined);
  const protectedIds = useRef<string[]>([]);

  useEffect(() => {
    events.current.clear();
    sessionEventKeyIds.current.clear();
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
      for (const event of values.values()) enqueueSessionEventIdentity(sessionEventKeyIds.current, event);
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
      batch.current?.enqueue(event);
    },
  });

  return { ...stream, events, sessionEventKeyIds, revision, overflowRevision, protectRequests };
}
