import type { ModelRouteView } from '../types.js';

const pageSize = 100;
const maximumPages = 100;

/** The same bounded keyset walk serves the complete grant catalog and focused route navigation. */
export async function loadModelRoutePages(
  tenant: string,
  signal: AbortSignal,
  fetchPage: (path: string, signal: AbortSignal) => Promise<ModelRouteView[]>,
  incompleteMessage: string,
  { focusRouteId, firstPageOnly = false }: { focusRouteId?: string; firstPageOnly?: boolean } = {},
) {
  const routes = new Map<string, ModelRouteView>();
  const visited = new Set<string>();
  let boundary: ModelRouteView | undefined;
  for (let index = 0; index < maximumPages; index += 1) {
    signal.throwIfAborted();
    const query = new URLSearchParams({ limit: String(pageSize) });
    if (tenant) query.set('tenant_external_id', tenant);
    if (boundary) {
      query.set('before_created_at', String(boundary.created_at));
      query.set('before_id', boundary.id);
    }
    const page = await fetchPage(`/internal/v1/model-routes?${query}`, signal);
    signal.throwIfAborted();
    if (page.length > pageSize) throw new Error(incompleteMessage);
    for (const route of page) routes.set(route.id, route);
    if (firstPageOnly || page.length < pageSize || (focusRouteId && page.some(route => route.id === focusRouteId))) return [...routes.values()];
    const last = page.at(-1)!;
    if (!Number.isSafeInteger(last.created_at) || !last.id) throw new Error(incompleteMessage);
    const cursor = `${last.created_at}\0${last.id}`;
    if (visited.has(cursor) || (boundary && (last.created_at > boundary.created_at || (last.created_at === boundary.created_at && last.id >= boundary.id)))) throw new Error(incompleteMessage);
    visited.add(cursor);
    boundary = last;
  }
  throw new Error(incompleteMessage);
}
