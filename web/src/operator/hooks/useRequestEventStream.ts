import { useEffect, useReducer, useRef } from 'react';
import { apiDiagnosticMessage, streamSse } from '../../api';
import { useI18n } from '../../i18n';
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
  const { t } = useI18n();
  const requestIdLabel = t('request.correlationId');
  const streamInterrupted = t('request.streamInterrupted');
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
    let connectedOnce = false;
    let controller: AbortController | undefined;
    const connect = async (activeController: AbortController) => {
      while (!activeController.signal.aborted) {
        dispatch(connectedOnce ? { type: 'reconnecting' } : { type: 'connecting' });
        try {
          await streamSse<RequestEvent>(
            `/internal/v1/request-events${query(tenant, cursor.current)}`,
            token,
            activeController.signal,
            ({ id, event: eventName, data: event }) => {
              if (activeController.signal.aborted) return;
              if (id !== event.event_id) throw new Error('SSE id does not match request event_id');
              if (eventName !== `request.${event.event_kind}`) throw new Error('SSE event name does not match request event_kind');
              if (!isAfter(event, cursor.current)) return;
              cursor.current = { eventAt: event.event_at, eventId: id };
              callback.current(event);
            },
            () => {
              if (activeController.signal.aborted) return;
              connectedOnce = true;
              dispatch({ type: 'live' });
            },
          );
          if (!activeController.signal.aborted) dispatch({ type: 'reconnecting' });
        } catch (reason) {
          if (!activeController.signal.aborted) {
            dispatch({
              type: 'reconnecting',
              message: apiDiagnosticMessage(reason, disconnectedMessage, { requestId: requestIdLabel, streamInterrupted }),
            });
          }
        }
        await waitForReconnect(activeController.signal, 1000);
      }
    };
    const start = () => {
      if (controller || document.hidden) return;
      controller = new AbortController();
      const activeController = controller;
      void connect(activeController).finally(() => {
        if (controller === activeController) controller = undefined;
      });
    };
    const visibilityChanged = () => {
      if (document.hidden) {
        const activeController = controller;
        controller = undefined;
        activeController?.abort();
        return;
      }
      start();
    };
    document.addEventListener('visibilitychange', visibilityChanged);
    start();
    return () => {
      document.removeEventListener('visibilitychange', visibilityChanged);
      const activeController = controller;
      controller = undefined;
      activeController?.abort();
    };
  }, [disconnectedMessage, enabled, tenant, token, requestIdLabel, streamInterrupted]);

  return {
    state: status.kind as SessionStreamState,
    error: status.kind === 'reconnecting' ? status.message : '',
  };
}
