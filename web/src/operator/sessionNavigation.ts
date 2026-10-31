import type { TypedFilterAst } from '../types.js';

const uuid = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

export function isUnlinkedSession(sessionId: string, unlinked = false) {
  return unlinked || sessionId.startsWith('unlinked:');
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
