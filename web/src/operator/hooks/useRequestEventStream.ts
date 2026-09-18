import { useEffect, useReducer, useRef } from 'react';
import { ApiError, apiDiagnosticMessage, streamSse } from '../../api';
import { useI18n } from '../../i18n';
import type { RequestEvent } from '../../types';
import type { SessionStreamState } from '../SessionMonitor';
import { requestEventReconnectDelayMs } from './requestEventStreamBackoff';

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
    let controller: AbortController | undefined;
    const connect = async (activeController: AbortController) => {
      // The counter belongs to this identity/visibility lifetime. A new
      // controller therefore starts fresh, while an opened stream clears the
      // failures accumulated before recovery.
      let connectedOnce = false;
      let reconnectAttempt = 0;
      let reconnectMessage = '';
      while (!activeController.signal.aborted) {
        dispatch(connectedOnce
          ? { type: 'reconnecting', message: reconnectMessage }
          : { type: 'connecting' });
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
              reconnectAttempt = 0;
              reconnectMessage = '';
              dispatch({ type: 'live' });
            },
          );
          if (!activeController.signal.aborted) {
            // A clean EOF is a reconnect boundary, not an operator-visible
            // error. The status label still explains that the stream is
            // reconnecting; failed attempts keep their diagnostic message.
            reconnectMessage = '';
            dispatch({ type: 'reconnecting' });
          }
        } catch (reason) {
          if (!activeController.signal.aborted) {
            if (reason instanceof ApiError && reason.code === 'sse_response_interrupted') {
              // The API classifies a 200-body interruption as recoverable.
              // Keep the reconnecting state visible without turning a normal
              // stream boundary into a persistent page-level error.
              reconnectMessage = '';
              dispatch({ type: 'reconnecting' });
            } else {
              reconnectMessage = apiDiagnosticMessage(reason, disconnectedMessage, { requestId: requestIdLabel, streamInterrupted });
              dispatch({
                type: 'reconnecting',
                message: reconnectMessage,
              });
            }
          }
        }
        if (activeController.signal.aborted) break;
        const delay = requestEventReconnectDelayMs(reconnectAttempt, Math.random());
        reconnectAttempt += 1;
        await waitForReconnect(activeController.signal, delay);
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
        // Do not leave a long-lived stream or backoff timer running while the
        // operator is in a background tab.
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
