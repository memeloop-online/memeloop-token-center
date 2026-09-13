export interface SessionIdentity {
  key_id: string;
  session_id: string;
}

export interface SessionEventIdentity {
  key_id: string;
  session_id: string | null;
  request_id: string;
  event_kind: 'started' | 'finished' | 'projected';
  status_code: number | null;
}

function identityKey(identity: SessionEventIdentity) {
  return JSON.stringify(identity);
}

export function sessionIdentityKey(session: SessionIdentity) {
  return JSON.stringify([session.key_id, session.session_id]);
}

export function requestEventSessionIdentity(event: {
  key_id: string;
  request_id: string;
  event_kind: 'started' | 'finished' | 'projected';
  status_code: number | null;
  session_context?: {
    association: string;
    session_id: string | null;
  } | null;
}): SessionEventIdentity | undefined {
  if (!event.key_id) return undefined;
  const context = event.session_context;
  const eventFields = { request_id: event.request_id, event_kind: event.event_kind, status_code: event.status_code };
  if (!context) return { key_id: event.key_id, session_id: null, ...eventFields };
  if (context.association === 'unlinked') {
    return { key_id: event.key_id, session_id: `unlinked:${event.key_id}`, ...eventFields };
  }
  return { key_id: event.key_id, session_id: context.session_id || null, ...eventFields };
}

export function enqueueSessionEventIdentity(queue: Set<string>, event: Parameters<typeof requestEventSessionIdentity>[0]) {
  const identity = requestEventSessionIdentity(event);
  if (identity) queue.add(identityKey(identity));
}

export function drainSessionEventIdentities(queue: Set<string>) {
  const drained = new Set(queue);
  queue.clear();
  return drained;
}

export function sessionEventTargetsSelection(eventIdentities: ReadonlySet<string>, selected?: SessionIdentity) {
  if (!selected) return false;
  return [...eventIdentities].some((value) => {
    const event = JSON.parse(value) as SessionEventIdentity;
    return event.key_id === selected.key_id && event.session_id === selected.session_id;
  });
}

export function sessionEventsRequireDetailRefresh(
  eventIdentities: ReadonlySet<string>,
  selected: SessionIdentity | undefined,
  detail: { session_id: string; requests: Array<{ request_id: string; status_code: number | null }> } | undefined,
) {
  if (!selected) return false;
  const relevant = [...eventIdentities]
    .map((value) => JSON.parse(value) as SessionEventIdentity)
    .filter((event) => event.key_id === selected.key_id && event.session_id === selected.session_id);
  if (relevant.length === 0) return false;
  if (!detail || detail.session_id !== selected.session_id) return true;
  return relevant.some((event) => {
    const recorded = detail.requests.find((request) => request.request_id === event.request_id);
    if (!recorded) return true;
    if (event.event_kind === 'started') return false;
    return recorded.status_code === null || recorded.status_code !== event.status_code;
  });
}

export function mergeSessionPage<T extends SessionIdentity>({
  current, page, firstPageSize, loadedOlder, older, background, state,
}: {
  current: T[];
  page: T[];
  firstPageSize: number;
  loadedOlder: boolean;
  older: boolean;
  background: boolean;
  state: '' | 'active' | 'has_errors';
}) {
  if (background && loadedOlder && state === 'active') {
    // The active result set is volatile. A tail row that just became terminal is
    // no longer returned by the server, so retaining a cached tail would create
    // a ghost. Reset to the authoritative first page and expose its cursor again.
    return { sessions: page, firstPageSize: page.length, loadedOlder: false };
  }
  if (background && loadedOlder) {
    const keys = new Set(page.map((session) => `${session.key_id}:${session.session_id}`));
    const oldTail = current.slice(firstPageSize)
      .filter((session) => !keys.has(`${session.key_id}:${session.session_id}`));
    return { sessions: [...page, ...oldTail], firstPageSize: page.length, loadedOlder: true };
  }
  if (background || !older) {
    return { sessions: page, firstPageSize: page.length, loadedOlder: false };
  }
  const known = new Set(current.map((session) => `${session.key_id}:${session.session_id}`));
  return {
    sessions: [...current, ...page.filter((session) => !known.has(`${session.key_id}:${session.session_id}`))],
    firstPageSize,
    loadedOlder: true,
  };
}
