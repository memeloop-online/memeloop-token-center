import { ApiError, api } from '../api.js';
import { transportProxyGroupCopy } from './transportProxyGroupCopy.js';

export interface TransportProxyMember {
  id: string;
  label: string;
  scheme: string;
  remote_dns: boolean;
  has_auth: boolean;
}

export interface TransportProxyGroup {
  id: string;
  tenant_external_id: string;
  name: string;
  version: number;
  members: TransportProxyMember[];
  bound_account_count: number;
}

export interface TransportProxyBinding {
  account_id: string;
  tenant_external_id: string;
  binding_version: number;
  group_id: string | null;
  group_version: number | null;
  initial_member_id: string | null;
  credential_generation: number;
  updated_at: number;
  runtime: {
    scope: 'this_process';
    configuration_state: 'unbound' | 'pending' | 'applied' | 'unavailable';
    selected_member_id: string | null;
    observed_at: number;
  };
}

export const transportProxyGroupsPath = '/internal/v1/transport-proxy-groups';
export const transportProxyRequestPolicy = { cache: 'no-store', credentials: 'omit', referrerPolicy: 'no-referrer' } as const;
export const transportProxyWaitMilliseconds = 15_000;

export function transportProxyFailureKind(reason: unknown): 'validation' | 'conflict' | 'denied' | 'unknown' {
  if (!(reason instanceof ApiError)) return 'unknown';
  if (reason.status === 400 || reason.status === 422) return 'validation';
  if (reason.status === 401 || reason.status === 403) return 'denied';
  if (reason.status === 404 || reason.status === 409) return 'conflict';
  return 'unknown';
}

export async function transportProxyRequest<Result>(path: string, token: string, init: RequestInit = {}): Promise<Result> {
  const controller = new AbortController();
  let timer: ReturnType<typeof setTimeout> | undefined;
  let stopWaiting: (() => void) | undefined;
  const interrupted = new Promise<never>((_, reject) => {
    stopWaiting = () => {
      controller.abort();
      reject(new Error('transport proxy request result unknown'));
    };
    timer = setTimeout(stopWaiting, transportProxyWaitMilliseconds);
    init.signal?.addEventListener('abort', stopWaiting, { once: true });
    if (init.signal?.aborted) stopWaiting();
  });
  try {
    return await Promise.race([
      api<Result>(path, token, { ...transportProxyRequestPolicy, ...init, signal: controller.signal }),
      interrupted,
    ]);
  } finally {
    clearTimeout(timer);
    if (stopWaiting) init.signal?.removeEventListener('abort', stopWaiting);
  }
}

export function transportProxyError(reason: unknown, locale = 'zh-CN', operation: 'read' | 'write' = 'write'): string {
  const copy = transportProxyGroupCopy(locale).errors;
  if (transportProxyFailureKind(reason) === 'unknown') return operation === 'read' ? copy.load : copy.unknown;
  if (!(reason instanceof ApiError)) return operation === 'read' ? copy.load : copy.unknown;
  switch (reason.code) {
    case 'proxy_group_version_conflict': return copy.groupConflict;
    case 'proxy_group_binding_conflict': return copy.bindingConflict;
    case 'proxy_group_in_use': return copy.inUse;
    case 'proxy_group_capacity_exceeded': return copy.capacity;
    case 'invalid_request': return copy.invalid;
  }
  if (reason.status === 401 || reason.status === 403) return copy.denied;
  if (reason.status === 400 || reason.status === 422) return copy.validation;
  if (reason.status === 404) return copy.missing;
  if (reason.status === 409) return copy.conflict;
  return operation === 'read' ? copy.load : copy.unknown;
}
