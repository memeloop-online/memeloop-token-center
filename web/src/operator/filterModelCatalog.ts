export const filterModelNameSuggestionLimit = 50;

/**
 * Suggestions for the request filter's model-name field. The endpoint returns
 * distinct public model names recorded for the scope — including names whose
 * routes are disabled or archived — with no route, account, or availability
 * enrichment. The backend applies a bounded, case-insensitive literal
 * substring search, so manual historical names and "contains" fragments
 * always remain valid even when no suggestion matches.
 */
export function filterModelNamesUrl(tenant: string, search: string, limit = filterModelNameSuggestionLimit): string {
  const params = new URLSearchParams({ limit: String(limit) });
  if (tenant) params.set('tenant_external_id', tenant);
  const query = search.trim();
  if (query) params.set('search', query);
  return `/internal/v1/filter-model-names?${params.toString()}`;
}
