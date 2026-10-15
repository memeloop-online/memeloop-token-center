import type { RequestEgressAttempt, RequestEgressSummary, SelectedRequestEgress } from '../../src/types.js';

declare global {
  interface Window { egressFixture: { mode: string; reads: string[]; release?: () => void; switchTenant: () => void } }
}

export const historicalExit: SelectedRequestEgress = {
  kind: 'proxy', group_id: 'group-a', group_version: 1, group_name: 'Research then',
  member_id: 'member-a', member_name: 'Original exit', scheme: 'socks5h', host: 'old.proxy.test', port: 1080,
};

export const egressSummaries: RequestEgressSummary[] = [
  { selected: historicalExit, recorded_attempts: 3, completed_attempts: 2, final_response_attempts: 1,
    request_known_attempts: 2, response_known_attempts: 1, request_payload_bytes: 1200, response_payload_bytes: 0,
    measurement_basis: 'application_payload_estimate', wire_bytes: null, supplier_billed_bytes: null },
  { selected: { ...historicalExit, group_version: 2, group_name: 'Research renamed', host: 'later.proxy.test' }, recorded_attempts: 1,
    completed_attempts: 1, final_response_attempts: 1, request_known_attempts: 0, response_known_attempts: 0,
    request_payload_bytes: null, response_payload_bytes: null, measurement_basis: 'application_payload_estimate', wire_bytes: null, supplier_billed_bytes: null },
  { selected: { kind: 'direct', group_id: null, group_version: null, group_name: null, member_id: null, member_name: null, scheme: null, host: null, port: null },
    recorded_attempts: 1, completed_attempts: 1, final_response_attempts: 1, request_known_attempts: 1, response_known_attempts: 1,
    request_payload_bytes: 0, response_payload_bytes: 2400, measurement_basis: 'application_payload_estimate', wire_bytes: null, supplier_billed_bytes: null },
];

export const egressAttempts: RequestEgressAttempt[] = ['attempt-a', 'attempt-b'].map((attempt_id, index) => ({
  request_id: 'request-a', attempt_id, outbound_attempt: 1, connect_attempt: 1, route_id: 'route-a', upstream_account_id: 'account-a',
  credential_generation: 1, transport_revision: 2, observed_at: 1000 + index, selected: historicalExit,
  transport: { unsupported_future_field: 'credential-canary-must-not-render' }, connection_binding: null,
  request_payload_bytes: index ? null : 100, response_payload_bytes: index ? 0 : null,
  completed: true, final_response: index === 1, response_body_complete: false, measurement_basis: 'application_payload_estimate',
}));
