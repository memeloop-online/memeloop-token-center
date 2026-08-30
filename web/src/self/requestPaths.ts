import type { LogicalSessionCursor, LogicalSessionDetail, RequestView } from '../types';

export const requestPageSize = 50;
export const sessionPageSize = 50;
export const sessionDetailPageSize = 100;

export interface RequestFilters {
  from: string;
  to: string;
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
}

export const emptyRequestFilters: RequestFilters = {
  from: '',
  to: '',
  model: '',
  protocol: '',
  status: '',
  errorCode: '',
  upstreamAccountId: '',
  routeId: '',
  minDurationMs: '',
  maxDurationMs: '',
  minCost: '',
  maxCost: '',
};

export function requestsPath(filters: RequestFilters, before?: RequestView, limit = requestPageSize) {
  const query = new URLSearchParams({ limit: String(limit) });
  const from = filters.from ? new Date(filters.from).getTime() : Number.NaN;
  const parsedTo = filters.to ? new Date(filters.to).getTime() : Number.NaN;
  const to = Number.isFinite(parsedTo) && /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}$/.test(filters.to)
    ? parsedTo + 59_999
    : parsedTo;
  if (Number.isFinite(from)) query.set('from_created_at', String(from));
  if (Number.isFinite(to)) query.set('to_created_at', String(to));
  if (filters.model.trim()) query.set('model', filters.model.trim());
  if (filters.protocol.trim()) query.set('protocol', filters.protocol.trim());
  if (filters.status) query.set('status', filters.status);
  if (filters.errorCode.trim()) query.set('error_code', filters.errorCode.trim());
  if (filters.upstreamAccountId.trim()) query.set('upstream_account_id', filters.upstreamAccountId.trim());
  if (filters.routeId.trim()) query.set('route_id', filters.routeId.trim());
  if (filters.minDurationMs.trim()) query.set('min_duration_ms', filters.minDurationMs.trim());
  if (filters.maxDurationMs.trim()) query.set('max_duration_ms', filters.maxDurationMs.trim());
  if (filters.minCost.trim()) query.set('min_cost', filters.minCost.trim());
  if (filters.maxCost.trim()) query.set('max_cost', filters.maxCost.trim());
  if (before) {
    query.set('before_created_at', String(before.created_at));
    query.set('before_id', before.request_id);
  }
  return `/self/v1/requests?${query}`;
}

export function statsPath(filters: RequestFilters) {
  const query = new URLSearchParams(requestsPath(filters).split('?')[1]);
  query.delete('limit');
  query.delete('before_created_at');
  query.delete('before_id');
  return `/self/v1/stats?${query}`;
}

export function sessionsPath(before?: LogicalSessionCursor, queryText?: string) {
  const query = new URLSearchParams({ limit: String(sessionPageSize) });
  if (queryText?.trim()) query.set('q', queryText.trim());
  if (before) {
    query.set('before_last_activity_at', String(before.before_last_activity_at));
    query.set('before_session_id', before.before_session_id);
  }
  return `/self/v1/sessions?${query}`;
}

export function sessionDetailPath(sessionId: string, cursor?: LogicalSessionDetail['next_cursor']) {
  const query = new URLSearchParams({ limit: String(sessionDetailPageSize) });
  if (cursor) {
    query.set('before_created_at', String(cursor.before_created_at));
    query.set('before_request_id', cursor.before_request_id);
  }
  return `/self/v1/sessions/${encodeURIComponent(sessionId)}?${query}`;
}
