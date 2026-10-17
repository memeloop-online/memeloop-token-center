import type { ModelRouteView } from '../types.js';

export const filterModelPageSize = 100;

/** The route endpoint is a cursor page, not the complete public model directory. */
export async function loadFilterModelRoutes(
  fetchPage: (cursor?: Pick<ModelRouteView, 'created_at' | 'id'>) => Promise<ModelRouteView[]>,
  onPage: (routes: ModelRouteView[]) => void,
  signal: AbortSignal,
): Promise<void> {
  const routes = new Map<string, ModelRouteView>();
  const cursors = new Set<string>();
  let cursor: ModelRouteView | undefined;
  while (!signal.aborted) {
    const page = await fetchPage(cursor);
    if (signal.aborted) return;
    for (const route of page) routes.set(route.id, route);
    onPage([...routes.values()]);
    if (page.length < filterModelPageSize) return;
    cursor = page[page.length - 1];
    const key = `${cursor.created_at}:${cursor.id}`;
    if (cursors.has(key)) throw new Error('Model catalog cursor did not advance');
    cursors.add(key);
  }
}
