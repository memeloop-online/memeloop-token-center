import { useEffect, useRef, useState } from 'react';
import type { RequestEvent } from '../../types';
import { enqueueSessionEventKey } from '../sessionRefresh';
import { useRequestEventStream } from './useRequestEventStream';

export function useOperatorRequestStream({ token, tenant, enabled, disconnectedMessage }: {
  token: string;
  tenant: string;
  enabled: boolean;
  disconnectedMessage: string;
}) {
  const events = useRef(new Map<string, RequestEvent>());
  const sessionEventKeyIds = useRef(new Set<string>());
  const [revision, setRevision] = useState(0);

  useEffect(() => {
    events.current.clear();
    sessionEventKeyIds.current.clear();
    setRevision((value) => value + 1);
  }, [tenant, token]);

  const stream = useRequestEventStream({
    token,
    tenant,
    enabled,
    disconnectedMessage,
    onEvent: (event) => {
      events.current.delete(event.request_id);
      events.current.set(event.request_id, event);
      while (events.current.size > 200) {
        const oldest = events.current.keys().next().value as string | undefined;
        if (!oldest) break;
        events.current.delete(oldest);
      }
      enqueueSessionEventKey(sessionEventKeyIds.current, event.key_id);
      setRevision((value) => value + 1);
    },
  });

  return { ...stream, events, sessionEventKeyIds, revision };
}
