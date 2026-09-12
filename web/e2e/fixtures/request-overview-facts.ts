import type { MonitoringUpstreamModel } from '../../src/types.js';

// Synthetic presentation facts only: provider labels do not establish real
// Copilot, Cursor or Kimi authentication, routing, billing or integration.
export const requestOverviewFacts: MonitoringUpstreamModel[] = [
  { name: 'Copilot', id: 'account-0', model: 'shared-model', requests: 10 },
  { name: 'Cursor', id: 'account-1', model: 'other-model', requests: 9 },
  { name: 'Kimi', id: 'account-2', model: 'shared-model', requests: 1 },
  // Conflicting duplicate pair: presentation must not silently pick a winner.
  { name: 'Copilot', id: 'account-0', model: 'shared-model', requests: 2 },
].map<MonitoringUpstreamModel>((row, index) => ({
  upstream_account_id: row.id, upstream_name: row.name, model: row.model,
  metrics: { requests: row.requests, successful_requests: row.requests - 1, failed_requests: 1,
    avg_duration_ms: 180 + index, p95_duration_ms: 420 + index,
    costs: [{ currency: index % 2 ? 'EUR' : 'USD', cost: `0.12345678${index}` }] },
  health: { version: 'upstream_breaker_v1', status: 'unknown', observed_at: null },
  terminal_outcomes: [{ id: `outcome-${index}`, source: 'request', created_at: index * 100,
    status: 'failure', duration_ms: 70 + index, error_code: `fixture-error-${index}` }],
}));
