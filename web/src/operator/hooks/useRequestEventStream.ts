import { useEffect, useReducer, useRef } from 'react';
import { streamSse } from '../../api';
import type { RequestEvent } from '../../types';
import type { SessionStreamState } from '../SessionMonitor';

interface EventCursor {
  eventAt: number;
  eventId: string;
}

type StreamStatus =
  | { kind: 'idle' }
  | { kind: 'connecting' }
  | { kind: 'live' }
  | { kind: 'reconnecting'; message: string };

type StreamAction =
  | { type: 'idle' }
  | { type: 'connecting' }
  | { type: 'live' }
  | { type: 'reconnecting'; message?: string };

function reducer(_state: StreamStatus, action: StreamAction): StreamStatus {
  switch (action.type) {
    case 'idle': return { kind: 'idle' };
    case 'connecting': return { kind: 'connecting' };
    case 'live': return { kind: 'live' };
    case 'reconnecting': return { kind: 'reconnecting', message: action.message ?? '' };
  }
}

function query(tenant: string, cursor?: EventCursor) {
  const params = new URLSearchParams();
  if (tenant) params.set('tenant_external_id', tenant);
  if (cursor) {
    params.set('after_event_at', String(cursor.eventAt));
    params.set('after_event_id', cursor.eventId);
  }
  const value = params.toString();
  return value ? `?${value}` : '';
}

function isAfter(event: RequestEvent, cursor?: EventCursor) {
  return !cursor || event.event_at > cursor.eventAt
    || (event.event_at === cursor.eventAt && event.event_id > cursor.eventId);
}

function waitForReconnect(signal: AbortSignal, milliseconds: number): Promise<void> {
  if (signal.aborted) return Promise.resolve();
  return new Promise((resolve) => {
    const timeout = window.setTimeout(finish, milliseconds);
    signal.addEventListener('abort', finish, { once: true });
    function finish() {
      window.clearTimeout(timeout);
      signal.removeEventListener('abort', finish);
      resolve();
    }
  });
}

export function useRequestEventStream({
  token,
  tenant,
  enabled,
  disconnectedMessage,
  onEvent,
}: {
  token: string;
  tenant: string;
  enabled: boolean;
  disconnectedMessage: string;
  onEvent: (event: RequestEvent) => void;
}) {
  const [status, dispatch] = useReducer(reducer, { kind: 'idle' });
  const cursor = useRef<EventCursor | undefined>(undefined);
  const callback = useRef(onEvent);
  callback.current = onEvent;

  useEffect(() => {
    cursor.current = undefined;
  }, [tenant, token]);

  useEffect(() => {
    if (!enabled || !token) {
      dispatch({ type: 'idle' });
      return;
    }
    const controller = new AbortController();
    let connectedOnce = false;
    const connect = async () => {
      while (!controller.signal.aborted) {
        dispatch(connectedOnce ? { type: 'reconnecting' } : { type: 'connecting' });
        try {
          await streamSse<RequestEvent>(
            `/internal/v1/request-events${query(tenant, cursor.current)}`,
            token,
            controller.signal,
            ({ id, event: eventName, data: event }) => {
              if (controller.signal.aborted) return;
              if (id !== event.event_id) throw new Error('SSE id does not match request event_id');
              if (eventName !== `request.${event.event_kind}`) throw new Error('SSE event name does not match request event_kind');
              if (!isAfter(event, cursor.current)) return;
              cursor.current = { eventAt: event.event_at, eventId: id };
              dispatch({ type: 'live' });
              callback.current(event);
            },
            () => {
              if (controller.signal.aborted) return;
              connectedOnce = true;
              dispatch({ type: 'live' });
            },
          );
          if (!controller.signal.aborted) dispatch({ type: 'reconnecting' });
        } catch (reason) {
          if (!controller.signal.aborted) {
            dispatch({
              type: 'reconnecting',
              message: reason instanceof Error ? reason.message : disconnectedMessage,
            });
          }
        }
        await waitForReconnect(controller.signal, 1000);
      }
    };
    void connect();
    return () => controller.abort();
  }, [disconnectedMessage, enabled, tenant, token]);

  return {
    state: status.kind as SessionStreamState,
    error: status.kind === 'reconnecting' ? status.message : '',
  };
}
