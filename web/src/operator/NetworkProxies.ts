import { ApiError } from '../api.js';
import { networkProxyCopy } from './NetworkProxyCopy.js';
import { transportProxyFailureKind } from './transportProxyGroups.js';

export interface NetworkProxyAddress {
  scheme: string;
  host: string;
  port: number;
  remote_dns: boolean;
  has_auth: boolean;
}

export interface NetworkProxyHealth {
  status: string;
  checked_at: number | null;
  latency_ms: number | null;
  scope: string;
}

export interface NetworkProxyProbe {
  enabled: boolean;
  target_url: string;
  interval_ms: number;
  attempts: number;
}

export interface NetworkProxy {
  id: string;
  tenant_external_id: string;
  name: string;
  version: number;
  address: NetworkProxyAddress;
  probe: NetworkProxyProbe;
  active_health: NetworkProxyHealth;
  group_ids?: string[];
  bound_account_count?: number;
  created_at: number;
  updated_at: number;
}

export interface NetworkProxyObservations {
  proxy_id: string;
  proxy_version: number;
  observed_at: number;
  window_start: number;
  window_end: number;
  coverage: string;
  scope: string;
  stale: boolean;
  active_health: NetworkProxyHealth;
  request_outcomes: {
    succeeded: number;
    provider_failed: number;
    transport_failed: number;
    sample_count: number;
    cancelled: number;
    unknown: number;
    pending: number;
    request_success_rate: number | null;
    transport_success_rate: number | null;
  };
  traffic: {
    measurement_basis: string;
    request_payload_bytes: number | null;
    response_payload_bytes: number | null;
    recorded_attempts: number | null;
    request_known_attempts: number | null;
    response_known_attempts: number | null;
    wire_bytes: number | null;
    supplier_billed_bytes: number | null;
  };
}

export interface NetworkProxyStrategy {
  id: string;
  version: string;
  schema: Record<string, unknown>;
  default: Record<string, unknown>;
}

export interface NetworkProxySelection {
  configuration_revision: number | null;
  account_id: string;
  tenant_external_id: string;
  selection_version: number;
  binding_version: number;
  credential_generation: number;
  account_updated_at: number;
  desired: {
    mode: string;
    proxy_id: string | null;
    group_id: string | null;
    plugin_id: string | null;
    plugin_version: string | null;
    config: Record<string, unknown>;
  };
  applied?: { pending?: boolean; scope?: string };
}

export interface ConcurrencySample {
  tenant_external_id: string;
  sample_id: string;
  source: string;
  upstream_account_id: string | null;
  state: string;
  lease_state: string;
  selected_egress: {
    kind: string;
    scheme?: string | null;
    host?: string | null;
    port?: number | null;
  } | null;
  egress_state: string;
}

export interface UpstreamConcurrencySnapshot {
  contract_version: string;
  stale: boolean;
  observed_at: number | null;
  samples_truncated?: boolean;
  samples: ConcurrencySample[];
}

export const networkProxiesPath = '/internal/v1/transport-proxies';
export const networkProxyStrategiesPath = '/internal/v1/transport-proxy-strategies';
export const upstreamConcurrencyPath = '/internal/v1/upstream-concurrency';

export function networkProxySelectionPath(accountId: string): string {
  return `/internal/v1/upstreams/${encodeURIComponent(accountId)}/transport-proxy-selection`;
}

export function networkProxyObservationsPath(id: string, tenant: string, start: number, end: number): string {
  return `${networkProxiesPath}/${encodeURIComponent(id)}/observations?${new URLSearchParams({
    tenant_external_id: tenant, start: String(start), end: String(end),
  })}`;
}

/** Count live request-owner leases whose unfinished egress attempt uses this
 *  exact address. Mirrors the backend network_proxy series attribution; the
 *  sanitized scheme/host/port match never involves credentials. */
export function networkProxyLiveConcurrency(snapshot: UpstreamConcurrencySnapshot, address: NetworkProxyAddress): number | undefined {
  if (snapshot.contract_version !== 'upstream_concurrency_v1' || snapshot.stale || snapshot.samples_truncated || !Array.isArray(snapshot.samples)) return undefined;
  let count = 0;
  for (const sample of snapshot.samples) {
    const egress = sample.selected_egress;
    if (sample.source !== 'request_owner' || sample.lease_state !== 'live' || sample.egress_state !== 'unfinished_attempt') continue;
    if (!egress || egress.kind !== 'proxy') continue;
    if (egress.scheme === address.scheme && egress.port === address.port
      && typeof egress.host === 'string' && egress.host.toLowerCase() === address.host.toLowerCase()) count += 1;
  }
  return count;
}

/** Client-side draft check only; the server stays the source of truth for
 *  whether an address is inside the approved private network. */
export function networkProxyAddressValid(value: string): boolean {
  const trimmed = value.trim();
  if (!trimmed || /\s/.test(trimmed)) return false;
  try {
    const url = new URL(trimmed);
    return trimmed.length <= 2048 && url.protocol === 'socks5h:' && Boolean(url.hostname)
      && (url.pathname === '' || url.pathname === '/') && !url.hash && !url.search && url.port !== '0';
  } catch {
    return false;
  }
}

export function networkProxyError(reason: unknown, locale = 'zh-CN', operation: 'read' | 'write' = 'write'): string {
  const copy = networkProxyCopy(locale).errors;
  if (transportProxyFailureKind(reason) === 'unknown' || !(reason instanceof ApiError)) {
    return operation === 'read' ? copy.load : copy.unknown;
  }
  switch (reason.code) {
    case 'network_proxy_name_conflict': return copy.nameConflict;
    case 'network_proxy_version_conflict': return copy.versionConflict;
    case 'network_proxy_in_use': return copy.inUse;
    case 'network_proxy_selection_version_conflict':
    case 'proxy_group_version_conflict': return copy.selectionConflict;
    case 'invalid_request': return copy.invalid;
  }
  if (reason.status === 401 || reason.status === 403) return copy.denied;
  if (reason.status === 400 || reason.status === 422) return copy.validation;
  if (reason.status === 404) return copy.missing;
  if (reason.status === 409) return copy.conflict;
  return operation === 'read' ? copy.load : copy.unknown;
}
