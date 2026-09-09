import type { MonitoringMetrics, MonitoringTerminalOutcome } from '../types';

export interface UpstreamAvailabilityWindow {
  contract_version: 'upstream_account_availability_v1';
  generated_at: number;
  tenant_external_id: string;
  from_created_at: number;
  to_created_at: number;
  granularity: 'hour' | 'day';
  latency_is_approximate: boolean;
  latency_method: string;
  accounts: {
    upstream_account_id: string;
    metrics: MonitoringMetrics;
    terminal_outcomes: MonitoringTerminalOutcome[];
  }[];
}

export function upstreamAvailabilityPath(tenant: string, now: number): string {
  if (!tenant.trim()) throw new Error('An explicit tenant is required');
  return `/internal/v1/upstream-availability?${new URLSearchParams({
    tenant_external_id: tenant,
    from_created_at: String(now - 86_400_000),
    to_created_at: String(now),
  })}`;
}
