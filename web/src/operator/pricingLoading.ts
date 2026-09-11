import type { ModelPriceView } from '../types.js';

export const MODEL_PRICE_PAGE_SIZE = 200;

export function modelPricePagePath(currency: string, afterModel = '') {
  const query = new URLSearchParams({
    currency,
    limit: String(MODEL_PRICE_PAGE_SIZE),
  });
  if (afterModel) query.set('after_model', afterModel);
  return `/internal/v1/model-prices?${query}`;
}

/**
 * Publishes the first bounded page immediately, then progressively appends
 * stable model-name keyset pages. The cursor guard prevents a malformed or
 * stale server page from turning a background load into an infinite loop.
 */
export async function loadModelPricePages(
  currency: string,
  signal: AbortSignal,
  fetchPage: (path: string, signal: AbortSignal) => Promise<ModelPriceView[]>,
  publish: (prices: ModelPriceView[]) => void,
) {
  let afterModel = '';
  let prices: ModelPriceView[] = [];
  const seenCursors = new Set<string>();
  for (;;) {
    const page = await fetchPage(modelPricePagePath(currency, afterModel), signal);
    if (signal.aborted) return prices;
    if (page.length === 0) return prices;
    const nextCursor = page[page.length - 1]?.model ?? '';
    // Do not impose JavaScript's Unicode ordering on the database cursor:
    // PostgreSQL deployments can use a different collation. Repeating a
    // cursor is the only condition that could make this client loop forever.
    if (!nextCursor || seenCursors.has(nextCursor)) {
      throw new Error('model price page did not advance its cursor');
    }
    seenCursors.add(nextCursor);
    prices = [...prices, ...page];
    publish(prices);
    if (page.length < MODEL_PRICE_PAGE_SIZE) return prices;
    afterModel = nextCursor;
  }
}
