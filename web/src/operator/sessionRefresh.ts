import type { RequestArchiveState, RequestEventKind, RequestSessionContext } from '../types.js';

// The realtime cadence still yields briefly so a burst of lifecycle events
// becomes one authoritative list/detail read instead of one read per event.
export const sessionEventRefreshDelayMs = 500;
export const maxSessionSummaryIdentities = 100;

export function sessionRefreshDelayMs(intervalMs: number) {
  return intervalMs <= 0 ? sessionEventRefreshDelayMs : Math.max(sessionEventRefreshDelayMs, intervalMs);
}

export interface SessionIdentity {
  key_id: string;
  session_id: string;
}

export interface SessionEventIdentity {
  key_id: string;
  session_id: string | null;
  association: RequestSessionContext['association'] | null;
  request_id: string;
  event_kind: RequestEventKind;
  status_code: number | null;
  archive_state: RequestArchiveState;
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
  event_kind: RequestEventKind;
  status_code: number | null;
  archive_state: RequestArchiveState;
  session_context?: {
    association: RequestSessionContext['association'];
    session_id: string | null;
  } | null;
}): SessionEventIdentity | undefined {
  if (!event.key_id) return undefined;
  const context = event.session_context;
  const eventFields = {
    request_id: event.request_id,
    event_kind: event.event_kind,
    status_code: event.status_code,
    archive_state: event.archive_state,
  };
  if (!context) return { key_id: event.key_id, session_id: null, association: null, ...eventFields };
  if (context.association === 'unlinked') {
    return { key_id: event.key_id, session_id: `unlinked:${event.key_id}`, association: 'unlinked', ...eventFields };
  }
  return {
    key_id: event.key_id, session_id: context.session_id || null, association: 'confirmed', ...eventFields,
  };
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

export function sessionSummaryTargets(eventIdentities: ReadonlySet<string>) {
  const targets = new Map<string, SessionIdentity>();
  let unknown = false;
  for (const value of eventIdentities) {
    const event = JSON.parse(value) as SessionEventIdentity;
    if (!event.session_id) {
      unknown = true;
      continue;
    }
    const target = { key_id: event.key_id, session_id: event.session_id };
    targets.set(sessionIdentityKey(target), target);
    if (event.event_kind === 'projected' && event.association === 'confirmed') {
      const formerUnlinked = { key_id: event.key_id, session_id: `unlinked:${event.key_id}` };
      targets.set(sessionIdentityKey(formerUnlinked), formerUnlinked);
    }
  }
  return {
    identities: [...targets.values()],
    requiresFullReload: unknown || targets.size > maxSessionSummaryIdentities,
  };
}

function compareSessionOrder(left: SessionIdentity & { last_activity_at: number }, right: SessionIdentity & { last_activity_at: number }) {
  return right.last_activity_at - left.last_activity_at
    || right.session_id.localeCompare(left.session_id)
    || right.key_id.localeCompare(left.key_id);
}

export function mergeIncrementalSessionSummaries<T extends SessionIdentity & { last_activity_at: number }>({
  current, updates, requested, firstPageSize, firstPageLimit, hasMore,
}: {
  current: T[];
  updates: T[];
  requested: SessionIdentity[];
  firstPageSize: number;
  firstPageLimit: number;
  hasMore: boolean;
}): { sessions: T[]; requiresFullReload: boolean } {
  const requestedKeys = new Set(requested.map(sessionIdentityKey));
  const updatesByKey = new Map(updates.map((summary) => [sessionIdentityKey(summary), summary]));
  const currentIndex = new Map(current.map((summary, index) => [sessionIdentityKey(summary), index]));
  if (current.slice(0, firstPageSize)
    .some((summary) => requestedKeys.has(sessionIdentityKey(summary)) && !updatesByKey.has(sessionIdentityKey(summary)))) {
    // An affected visible row disappeared or stopped matching the active
    // server-side filters. Only a new first-page query can fill that vacancy.
    return { sessions: current, requiresFullReload: true };
  }

  const firstPage = current.slice(0, firstPageSize);
  const boundary = firstPage.at(-1);
  for (const update of updates) {
    const index = currentIndex.get(sessionIdentityKey(update));
    if (index === undefined) {
      if (firstPageSize < firstPageLimit || !boundary || compareSessionOrder(update, boundary) < 0) {
        return { sessions: current, requiresFullReload: true };
      }
      continue;
    }
    if (index < firstPageSize && hasMore && boundary && compareSessionOrder(update, boundary) > 0) {
      // A first-page row moving below the previous boundary can admit an
      // unseen row. Exact summaries cannot prove which row should replace it.
      return { sessions: current, requiresFullReload: true };
    }
    if (index >= firstPageSize && boundary && compareSessionOrder(update, boundary) < 0) {
      // A loaded tail row became recent enough to enter the first page.
      return { sessions: current, requiresFullReload: true };
    }
  }

  const replaced = current
    .filter((summary, index) => index < firstPageSize || !requestedKeys.has(sessionIdentityKey(summary))
      || updatesByKey.has(sessionIdentityKey(summary)))
    .map((summary) => updatesByKey.get(sessionIdentityKey(summary)) ?? summary);
  const sortedFirstPage = replaced.slice(0, firstPageSize).sort(compareSessionOrder);
  return { sessions: [...sortedFirstPage, ...replaced.slice(firstPageSize)], requiresFullReload: false };
}

export function sessionEventTargetsSelection(eventIdentities: ReadonlySet<string>, selected?: SessionIdentity) {
  if (!selected) return false;
  return [...eventIdentities].some((value) => {
    const event = JSON.parse(value) as SessionEventIdentity;
    if (event.key_id !== selected.key_id) return false;
    if (event.session_id === selected.session_id) return true;
    // A confirmed projection removes the request from its former credential-
    // owned unlinked aggregate. The event exposes only the new identity, so
    // explicitly invalidate the old aggregate without touching another named
    // session on the same credential.
    return event.event_kind === 'projected' && event.association === 'confirmed' && event.session_id !== null
      && selected.session_id === `unlinked:${selected.key_id}`;
  });
}

export function sessionEventsRequireDetailRefresh(
  eventIdentities: ReadonlySet<string>,
  selected: SessionIdentity | undefined,
  detail: {
    session_id: string;
    requests: Array<{
      request_id: string;
      status_code: number | null;
      archive_state?: RequestArchiveState;
      session_context?: RequestSessionContext | null;
    }>;
  } | undefined,
) {
  if (!selected) return false;
  const relevant = [...eventIdentities]
    .map((value) => JSON.parse(value) as SessionEventIdentity)
    .filter((event) => event.key_id === selected.key_id && (event.session_id === selected.session_id
      || (event.event_kind === 'projected' && event.association === 'confirmed' && event.session_id !== null
        && selected.session_id === `unlinked:${selected.key_id}`)));
  if (relevant.length === 0) return false;
  if (!detail || detail.session_id !== selected.session_id) return true;
  return relevant.some((event) => {
    const recorded = detail.requests.find((request) => request.request_id === event.request_id);
    if (!recorded) return true;
    if (event.event_kind === 'started') return false;
    if (event.event_kind === 'archive_bound' || event.event_kind === 'archive_gap') {
      // Request and response spools transition independently. Their combined
      // archive_state can remain `pending` after the first side binds or gaps,
      // so the transition event itself is the convergence signal. The SSE
      // cursor deduplicates replay and each purpose emits only on transition.
      return true;
    }
    if (event.event_kind === 'projected') {
      if (selected.session_id === `unlinked:${selected.key_id}`) return true;
      return recorded.session_context?.association !== 'confirmed'
        || recorded.session_context.session_id !== selected.session_id;
    }
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
