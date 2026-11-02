import type { NetworkProxy, NetworkProxyObservations, NetworkProxySelection, NetworkProxyStrategy, UpstreamConcurrencySnapshot } from '../../src/operator/NetworkProxies.js';

export const csilProxy: NetworkProxy = {
  id: '10000000-0000-4000-8000-000000000001', tenant_external_id: 'fixture', name: 'CSIL', version: 1,
  address: { scheme: 'socks5h', host: '100.64.0.16', port: 1080, remote_dns: true, has_auth: false },
  probe: { enabled: true, target_url: 'https://openai.com/', interval_ms: 60_000, attempts: 2 },
  active_health: { status: 'healthy', checked_at: 1_793_510_400_000, latency_ms: 42, scope: 'process' },
  group_ids: [], bound_account_count: 0, created_at: 1_793_510_000_000, updated_at: 1_793_510_000_000,
};

export const networkProxyStrategies: NetworkProxyStrategy[] = [
  { id: 'manual', version: '1', schema: { type: 'object', additionalProperties: false }, default: {} },
  { id: 'auto', version: '1', schema: { type: 'object', additionalProperties: false, properties: {
    minimum_samples: { type: 'integer', minimum: 1, maximum: 10_000 },
    minimum_dwell_ms: { type: 'integer', minimum: 0, maximum: 3_600_000 },
    health_max_age_ms: { type: 'integer', minimum: 10_000, maximum: 3_600_000 },
    recovery_successes: { type: 'integer', minimum: 1, maximum: 8 },
    improvement_threshold: { type: 'number', minimum: 0, maximum: 1 },
    signal_window_ms: { type: 'integer', minimum: 60_000, maximum: 86_400_000 },
  } }, default: {
    minimum_samples: 8, minimum_dwell_ms: 30_000, health_max_age_ms: 180_000,
    recovery_successes: 2, improvement_threshold: 0.2, signal_window_ms: 3_600_000,
  } },
  { id: 'fixture-latency', version: '2', schema: { type: 'object', additionalProperties: false, properties: {
    minimum_samples: { type: 'integer', minimum: 1, maximum: 100 },
    prefer_healthy: { type: 'boolean' },
  } }, default: { minimum_samples: 4, prefer_healthy: true } },
];

export const legacySelection: NetworkProxySelection = {
  configuration_revision: null, account_id: 'account-native', tenant_external_id: 'fixture', selection_version: 0,
  binding_version: 0, credential_generation: 7, account_updated_at: 100,
  desired: { mode: 'legacy', proxy_id: null, group_id: null, plugin_id: null, plugin_version: null, config: {} },
  applied: { pending: false, scope: 'this_process' },
};

export function proxyObservations(proxy: NetworkProxy): NetworkProxyObservations {
  return {
    proxy_id: proxy.id, proxy_version: proxy.version, observed_at: 1_793_510_500_000,
    window_start: 1_793_424_100_000, window_end: 1_793_510_500_000,
    coverage: 'recorded_codex_attempts_with_proxy_version', scope: 'persisted_observations', stale: false,
    active_health: proxy.active_health,
    request_outcomes: {
      succeeded: 18, provider_failed: 1, transport_failed: 1, sample_count: 20,
      cancelled: 0, unknown: 0, pending: 0, request_success_rate: 0.9, transport_success_rate: 0.95,
    },
    traffic: {
      measurement_basis: 'application_payload_estimate', request_payload_bytes: 4096, response_payload_bytes: 16_384,
      recorded_attempts: 20, request_known_attempts: 20, response_known_attempts: 20, wire_bytes: null, supplier_billed_bytes: null,
    },
  };
}

export const concurrencySnapshot: UpstreamConcurrencySnapshot = {
  contract_version: 'upstream_concurrency_v1', stale: false, observed_at: 1_793_510_500_000, samples_truncated: false,
  samples: [
    { tenant_external_id: 'fixture', sample_id: 'request-live', source: 'request_owner', upstream_account_id: 'account-native', state: 'unknown', lease_state: 'live', egress_state: 'unfinished_attempt',
      selected_egress: { kind: 'proxy', scheme: 'socks5h', host: '100.64.0.16', port: 1080 } },
    { tenant_external_id: 'fixture', sample_id: 'request-complete', source: 'request_owner', upstream_account_id: 'account-native', state: 'unknown', lease_state: 'live', egress_state: 'completed_attempt',
      selected_egress: { kind: 'proxy', scheme: 'socks5h', host: '100.64.0.16', port: 1080 } },
    { tenant_external_id: 'fixture', sample_id: 'request-other', source: 'request_owner', upstream_account_id: 'account-native', state: 'unknown', lease_state: 'live', egress_state: 'unfinished_attempt',
      selected_egress: { kind: 'proxy', scheme: 'socks5h', host: '10.20.30.40', port: 1080 } },
  ],
};
