export interface SessionIdentity {
  key_id: string;
  session_id: string;
}

export function enqueueSessionEventKey(queue: Set<string>, keyId: string) {
  if (keyId) queue.add(keyId);
}

export function drainSessionEventKeys(queue: Set<string>) {
  const drained = new Set(queue);
  queue.clear();
  return drained;
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
