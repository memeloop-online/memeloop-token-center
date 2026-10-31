import type { TypedFilterAst } from '../types.js';

export interface SessionRequestOrigin {
  route: 'overview' | 'requests';
  requestId?: string;
  filters?: TypedFilterAst;
  openDetail?: boolean;
  scrollY: number;
}

const uuid = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

export function isUnlinkedSession(sessionId: string, unlinked = false) {
  return unlinked || sessionId.startsWith('unlinked:');
}

export function restoreRequestSessionOrigin(origin: SessionRequestOrigin) {
  if (!origin.requestId) {
    window.scrollTo(0, origin.scrollY);
    return;
  }
  let remainingFrames = 300;
  const restore = () => {
    const target = origin.openDetail
      ? document.querySelector<HTMLElement>('.request-diagnostic-session')
      : [...document.querySelectorAll<HTMLElement>('tr[data-request-id]')]
        .find(row => row.dataset.requestId === origin.requestId)
        ?.querySelector<HTMLElement>('.request-session-cell button') ?? null;
    if (target) {
      window.scrollTo(0, origin.scrollY);
      target.focus({ preventScroll: true });
      return;
    }
    remainingFrames -= 1;
    if (remainingFrames > 0) requestAnimationFrame(restore);
  };
  requestAnimationFrame(restore);
}

export function unlinkedRequestFilter(
  sessionId: string,
  keyId?: string,
  source: TypedFilterAst = { logical_operator: 'and', conditions: [] },
): TypedFilterAst | undefined {
  const embeddedKey = sessionId.startsWith('unlinked:') ? sessionId.slice('unlinked:'.length) : undefined;
  const credential = keyId ?? embeddedKey;
  if (!credential || !uuid.test(credential) || (embeddedKey && embeddedKey !== credential) || source.logical_operator !== 'and') return undefined;
  return {
    logical_operator: 'and',
    conditions: [
      ...source.conditions.filter(condition => condition.field !== 'key_id' && condition.field !== 'session_association'),
      { field: 'key_id', operator: 'equals', value: { type: 'uuid', value: credential } },
      { field: 'session_association', operator: 'equals', value: { type: 'text', value: 'unlinked' } },
    ],
  };
}
