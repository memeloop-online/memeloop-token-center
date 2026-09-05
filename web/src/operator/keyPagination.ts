import type { KeyListCursor, KeyListPage, KeyView } from '../types';

/**
 * Keep the rendered page smaller than the API maximum and ask for one
 * additional row.  The extra row proves that another page exists without
 * guessing from a page that happens to be full.
 */
export const keyListPageSize = 100;
const keyListFetchLimit = keyListPageSize + 1;

export function keyListPath(tenant: string, cursor?: KeyListCursor) {
  const query = new URLSearchParams({ limit: String(keyListFetchLimit) });
  if (tenant) query.set('tenant_external_id', tenant);
  if (cursor) {
    query.set('before_created_at', String(cursor.before_created_at));
    query.set('before_id', cursor.before_id);
  }
  return `/internal/v1/keys?${query}`;
}

/**
 * The control API intentionally keeps its established array response.  Its
 * exclusive `(created_at, key_id)` keyset is carried in the next request, so
 * one look-ahead row is enough to expose a truthful "load more" affordance.
 */
export function keyListPage(rows: KeyView[]): KeyListPage {
  const values = rows.slice(0, keyListPageSize);
  const last = values.at(-1);
  return {
    values,
    nextCursor: rows.length > keyListPageSize && last
      ? { before_created_at: last.created_at, before_id: last.key_id }
      : undefined,
  };
}

/** Preserve server order while protecting the UI from an accidental replay. */
export function appendDistinctKeys(current: KeyView[], incoming: KeyView[]) {
  const seen = new Set(current.map((value) => value.key_id));
  return [...current, ...incoming.filter((value) => !seen.has(value.key_id))];
}

function cursorFor(value: Pick<KeyView, 'created_at' | 'key_id'>): KeyListCursor {
  return { before_created_at: value.created_at, before_id: value.key_id };
}

function sameCursor(left: KeyListCursor, right: KeyListCursor) {
  return left.before_created_at === right.before_created_at && left.before_id === right.before_id;
}

/** Returns true only when `candidate` is strictly older in the API sort order. */
export function isOlderKeyCursor(candidate: KeyListCursor, boundary: KeyListCursor) {
  return candidate.before_created_at < boundary.before_created_at
    || (candidate.before_created_at === boundary.before_created_at && candidate.before_id < boundary.before_id);
}

export type KeyPageApplication =
  | { ok: true; values: KeyView[]; nextCursor?: KeyListCursor }
  | { ok: false };

/**
 * Refuse malformed or non-advancing pages instead of leaving a "load more"
 * button that can repeatedly request the same cursor forever.
 */
export function applyKeyPage(
  current: KeyView[],
  rows: KeyView[],
  requestedCursor?: KeyListCursor,
): KeyPageApplication {
  const page = keyListPage(rows);
  const cursors = rows.map(cursorFor);
  if (new Set(rows.map((value) => value.key_id)).size !== rows.length) return { ok: false };
  if (cursors.some((value, index) => index > 0 && !isOlderKeyCursor(value, cursors[index - 1]!))) return { ok: false };
  if (requestedCursor) {
    const last = current.at(-1);
    if (!last || !sameCursor(cursorFor(last), requestedCursor)) return { ok: false };
    if (page.values.some((value) => !isOlderKeyCursor(cursorFor(value), requestedCursor))) return { ok: false };
    if (page.nextCursor && !isOlderKeyCursor(page.nextCursor, requestedCursor)) return { ok: false };
  }
  const values = appendDistinctKeys(current, page.values);
  if (requestedCursor && page.values.length > 0 && values.length === current.length) return { ok: false };
  return { ok: true, values, nextCursor: page.nextCursor };
}

export type KeyListLoadState = 'idle' | 'initial-loading' | 'loading-more' | 'more' | 'complete' | 'failed';

export interface KeyListRequestIdentity {
  generation: number;
  scopeGeneration: number;
}

export function ownsKeyListRequest(
  active: KeyListRequestIdentity | undefined,
  candidate: KeyListRequestIdentity,
) {
  return active?.generation === candidate.generation
    && active.scopeGeneration === candidate.scopeGeneration;
}

export function canLoadMoreKeys(
  state: KeyListLoadState,
  hasNextCursor: boolean,
  hasActiveRequest: boolean,
) {
  return state === 'more' && hasNextCursor && !hasActiveRequest;
}

export type CredentialListPresentation = 'loading' | 'loading-more' | 'failed' | 'filtered' | 'more' | 'complete';

export function credentialListPresentation(state: KeyListLoadState, hasFilters: boolean): CredentialListPresentation {
  if (state === 'initial-loading' || state === 'idle') return 'loading';
  if (state === 'loading-more') return 'loading-more';
  if (state === 'failed') return 'failed';
  if (hasFilters) return 'filtered';
  return state === 'more' ? 'more' : 'complete';
}

/** Route editors require a selected tenant; the key list does not. */
export function shouldLoadCredentialRoutes(tenant: string) {
  return Boolean(tenant.trim());
}

/** A global management token may read a key by its unambiguous stable ID. */
export function canReadCredentialLimits(token: string) {
  return Boolean(token.trim());
}

/** All credential mutations require choosing one tenant in the operator UI. */
export function canWriteCredential(tenant: string) {
  return Boolean(tenant.trim());
}

export function matchesCredentialSearch(value: KeyView, search: string, locale: string) {
  const needle = search.trim().toLocaleLowerCase(locale);
  if (!needle) return true;
  return [value.alias, value.principal_external_id]
    .filter((candidate): candidate is string => Boolean(candidate))
    .some((candidate) => candidate.toLocaleLowerCase(locale).includes(needle));
}

export const credentialStatuses = ['active', 'suspended', 'revoked'] as const;

export function matchesCredentialStatus(value: KeyView, status: string) {
  return status === 'all' || (value.status ?? 'active') === status;
}
