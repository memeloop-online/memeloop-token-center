import type { RequestEvent, RequestListCursor, RequestView, TypedFilterAst } from '../../types.js';

export const emptyTypedFilterAst: TypedFilterAst = { logical_operator: 'and', conditions: [] };

export function typedFiltersActive(ast: TypedFilterAst) {
  return ast.conditions.length > 0;
}

/**
 * A summary of the records currently visible in the operator traffic view.
 *
 * This deliberately stays a view summary: pagination and live events can
 * mean it represents fewer records than the full query result.  Terminal
 * status is the same status predicate used by the shared request table, so
 * success-rate and latency never treat an in-flight request as a failure.
 */
export interface VisibleRequestTrafficSummary {
  requests: number;
  successful: number;
  failed: number;
  running: number;
  successRate: number | null;
  averageDurationMs: number | null;
}

export function summarizeVisibleRequests(requests: readonly RequestView[]): VisibleRequestTrafficSummary {
  let successful = 0;
  let failed = 0;
  let running = 0;
  let durationTotal = 0;
  let durationCount = 0;

  for (const request of requests) {
    if (request.status_code === null) {
      running += 1;
      continue;
    }
    if (request.status_code < 400) successful += 1;
    else failed += 1;
    if (request.duration_ms !== null && Number.isFinite(request.duration_ms)) {
      durationTotal += request.duration_ms;
      durationCount += 1;
    }
  }

  const terminal = successful + failed;
  return {
    requests: requests.length,
    successful,
    failed,
    running,
    successRate: terminal > 0 ? successful / terminal : null,
    averageDurationMs: durationCount > 0 ? durationTotal / durationCount : null,
  };
}

export function typedRequestQueryBody(tenant: string, ast: TypedFilterAst, before?: RequestListCursor) {
  return {
    tenant_external_id: tenant || undefined,
    limit: 100,
    paged: true,
    before_created_at: before?.before_created_at,
    before_id: before?.before_id,
    ast,
  };
}

export interface RequestFilters {
  from: string;
  to: string;
  keyId: string;
  model: string;
  protocol: string;
  status: string;
  errorCode: string;
  upstreamAccountId: string;
  routeId: string;
  minDurationMs: string;
  maxDurationMs: string;
  minCost: string;
  maxCost: string;
  keyAlias: string;
  principal: string;
}

export const emptyRequestFilters: RequestFilters = {
  from: '', to: '', keyId: '', model: '', protocol: '', status: '', errorCode: '', upstreamAccountId: '',
  routeId: '', minDurationMs: '', maxDurationMs: '', minCost: '', maxCost: '', keyAlias: '', principal: '',
};

export function requestQuery(tenant: string, filters: RequestFilters, before?: RequestListCursor) {
  const params = new URLSearchParams({ limit: '100', paged: 'true' });
  if (tenant) params.set('tenant_external_id', tenant);
  const from = filters.from ? Date.parse(filters.from) : Number.NaN;
  const to = filters.to ? Date.parse(filters.to) : Number.NaN;
  if (Number.isFinite(from)) params.set('from_created_at', String(from));
  if (Number.isFinite(to)) params.set('to_created_at', String(to));
  if (filters.keyId.trim()) params.set('key_id', filters.keyId.trim());
  if (filters.model.trim()) params.set('model', filters.model.trim());
  if (filters.protocol) params.set('protocol', filters.protocol);
  if (filters.status) params.set('status', filters.status);
  if (filters.errorCode.trim()) params.set('error_code', filters.errorCode.trim());
  if (filters.upstreamAccountId) params.set('upstream_account_id', filters.upstreamAccountId);
  if (filters.routeId.trim()) params.set('route_id', filters.routeId.trim());
  if (filters.minDurationMs.trim()) params.set('min_duration_ms', filters.minDurationMs.trim());
  if (filters.maxDurationMs.trim()) params.set('max_duration_ms', filters.maxDurationMs.trim());
  if (filters.minCost.trim()) params.set('min_cost', filters.minCost.trim());
  if (filters.maxCost.trim()) params.set('max_cost', filters.maxCost.trim());
  if (filters.keyAlias.trim()) params.set('key_alias', filters.keyAlias.trim());
  if (filters.principal.trim()) params.set('principal', filters.principal.trim());
  if (before) {
    params.set('before_created_at', String(before.before_created_at));
    params.set('before_id', before.before_id);
  }
  return `?${params}`;
}

export function filtersActive(filters: RequestFilters) {
  return Object.values(filters).some(Boolean);
}

export function requestViewFromEvent(event: RequestEvent, previous?: RequestView): RequestView {
  return {
    request_id: event.request_id,
    created_at: previous?.created_at ?? event.event_at,
    protocol: event.protocol,
    model: event.model,
    status_code: event.status_code,
    duration_ms: event.duration_ms,
    input_tokens: event.input_tokens,
    cached_input_tokens: previous?.cached_input_tokens ?? 0,
    cache_write_tokens: previous?.cache_write_tokens ?? 0,
    output_tokens: event.output_tokens,
    cost: event.cost,
    error_code: event.error_code,
    session_context: previous?.session_context ?? null,
  };
}

export function mergeLiveRequestEvents(
  snapshot: RequestView[],
  liveEvents: Map<string, RequestEvent>,
  preserveAll = false,
) {
  const merged = new Map(snapshot.map((request) => [request.request_id, request]));
  for (const event of liveEvents.values()) {
    merged.set(event.request_id, requestViewFromEvent(event, merged.get(event.request_id)));
  }
  // Keep any history page the operator deliberately loaded. The original
  // first page remains bounded at 100 when another server page exists, while
  // live events can displace only its oldest visible row; callers then advance
  // their keyset cursor from that actual visible tail. When the server says
  // this is the final page, retain every row so an incoming event cannot hide
  // the old final record behind a disabled Load older action.
  const visibleLimit = preserveAll ? merged.size : Math.max(100, snapshot.length);
  return [...merged.values()]
    // Match the database's complete descending keyset order. Millisecond
    // timestamps collide under concurrent traffic, so falling back to map
    // insertion order here could advance the next page cursor past a row the
    // UI never displayed.
    .sort((left, right) => right.created_at - left.created_at
      || right.request_id.localeCompare(left.request_id))
    .slice(0, visibleLimit);
}
