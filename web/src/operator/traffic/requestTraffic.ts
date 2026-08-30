import type { RequestEvent, RequestView } from '../../types';

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

export function requestQuery(tenant: string, filters: RequestFilters, before?: RequestView) {
  const params = new URLSearchParams({ limit: '100' });
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
    params.set('before_created_at', String(before.created_at));
    params.set('before_id', before.request_id);
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
    output_tokens: event.output_tokens,
    cost: event.cost,
    error_code: event.error_code,
    session_context: previous?.session_context ?? null,
  };
}

export function mergeLiveRequestEvents(snapshot: RequestView[], liveEvents: Map<string, RequestEvent>) {
  const merged = new Map(snapshot.map((request) => [request.request_id, request]));
  for (const event of liveEvents.values()) {
    merged.set(event.request_id, requestViewFromEvent(event, merged.get(event.request_id)));
  }
  return [...merged.values()]
    .sort((left, right) => right.created_at - left.created_at)
    .slice(0, 100);
}
