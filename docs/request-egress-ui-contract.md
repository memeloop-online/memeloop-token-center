# PR597 operator UI handoff

This is a read-only DTO handoff for the selected Kimi proxy-group UI owner. Do not infer historical selections from current group configuration. No UI files are owned by the backend change.

## Stable sources and endpoints

- Rust DTOs: `src/db/routing/egress.rs` (`SelectedEgress`, `RequestEgressAttempt`, `RequestEgressSummary`).
- Request-detail extension: `src/model.rs::RequestDetail.egress_attempts`; handler `src/api/control_requests.rs`; routes `src/api/routes/control.rs`.
- OpenAPI: `openapi/openapi.yaml`, schemas `SelectedRequestEgress`, `RequestEgressAttempt`, `RequestEgressSummary`, `RequestEgressSummaryResponse`.
- Storage: `migrations/common/0124_request_egress_attempts.sql`. Schema124 belongs to PR597, not the schema123/f545 release.
- `GET /internal/v1/requests/{request_id}` returns `egress_attempts` for authorized operators. Self-service omits this field. Authorization remains `requests:read` plus existing tenant ownership checks.
- `GET /internal/v1/requests/egress-summary?start=<unix-ms>&end=<unix-ms>&tenant_external_id=<optional>` returns the summary below. The half-open window `[start, end)` uses attempt start time. Tenant scope is enforced server-side. There is no group/member query parameter; select returned snapshots by their IDs, not labels.
- Current group configuration: `GET /internal/v1/transport-proxy-groups?tenant_external_id=...` and `GET /internal/v1/transport-proxy-groups/{group_id}?tenant_external_id=...`. See `src/db/transport_proxy_management/types.rs::view` and existing OpenAPI management contracts. Keep the existing access check and permissions; request-read access does not grant group-management authority.

## Response DTOs

Self-service `GET /self/v1/requests`, operator `GET /internal/v1/requests`, and
`POST /internal/v1/requests/query` include nullable `egress: { selected,
observed_at }`. `selected` uses the frozen `SelectedRequestEgress` below;
`observed_at` is the persisted attempt observation time in Unix milliseconds.
The server loads attempts for the authorized native request page in one batch,
choosing `final_response` first, then descending `observed_at`, then descending
`attempt_id` for deterministic ties. It does not consult current proxy groups or
perform connectivity probes. No attempt, malformed selected snapshot, generation
history, and imported archive history yield null. Other projections of the
shared request DTO can also return null when egress was not loaded.

Self-service retains key ownership; operator lists retain their existing
`requests:read` and tenant restrictions. Historical host, port, and human labels
are available within that request scope; URL userinfo, path, query, and fragment
are excluded by snapshot capture. The full attempt/transport diagnostics remain
operator-only. An observation is not evidence of current connectivity or delivery.

UUIDs are strings; timestamps are Unix milliseconds; byte counters are nullable nonnegative integers. The API does not send BigInt strings.

```ts
type SelectedRequestEgress = {
  kind: "proxy" | "direct" | "unknown";
  group_id: string | null;
  group_version: number | null;
  group_name: string | null;
  member_id: string | null;
  member_name: string | null;
  scheme: string | null;
  host: string | null;
  port: number | null;
};

type RequestEgressAttempt = {
  request_id: string;
  attempt_id: string;
  outbound_attempt: number;
  connect_attempt: number;
  route_id: string;
  upstream_account_id: string;
  credential_generation: number;
  transport_revision: number;
  observed_at: number;
  selected: SelectedRequestEgress;
  transport: Record<string, unknown>;
  connection_binding: Record<string, unknown> | null;
  request_payload_bytes: number | null;
  response_payload_bytes: number | null;
  completed: boolean;
  final_response: boolean;
  response_body_complete: boolean;
  measurement_basis: "application_payload_estimate";
};

type RequestEgressSummary = {
  selected: SelectedRequestEgress;
  recorded_attempts: number;
  completed_attempts: number;
  final_response_attempts: number;
  request_known_attempts: number;
  response_known_attempts: number;
  request_payload_bytes: number | null;
  response_payload_bytes: number | null;
  measurement_basis: "application_payload_estimate";
  wire_bytes: null;
  supplier_billed_bytes: null;
};

type RequestEgressSummaryResponse = {
  start: number;
  end: number;
  coverage: "recorded_codex_attempts_only";
  measurement_basis: "application_payload_estimate";
  wire_bytes: null;
  supplier_billed_bytes: null;
  groups: RequestEgressSummary[];
};

type RequestDetailEgressExtension = {
  egress_attempts?: RequestEgressAttempt[];
};
```

Current management group members expose `id`, `label`, `scheme`, `host`, `port`, `remote_dns`, `has_auth`. They do **not** return `proxy_url`, username or password. Group `id/name/version` and member `id/label` describe current configuration, unlike attempt `selected.group_id/group_name/group_version/member_id/member_name`, which describe the frozen actual selection.

## Display rules

- Show protocol, host and port to authorized administrators. Do not replace an available host with a mask. URL authentication is excluded; never reconstruct credentials or use a credential-bearing write DTO for display.
- Label `direct` as “直连”; missing history, empty attempt arrays and unknown fields as “未记录”. An absent self-service field does not mean zero sends. `null` bytes mean unrecorded, not zero; `0` means an observed zero. API errors are load failures, not zero traffic.
- Use `attempt_id` as the row key. HTTP400 and native H2 recovery can repeat the routing counters while producing distinct sends. Show all recorded attempts and distinguish `final_response`; do not count only the final response as total attempted payload.
- `completed` means terminated, including error/cancellation; it does not mean success. `response_body_complete` requires upstream EOF. A final response can still be a rejection or interrupted stream. Keep the existing `failure_diagnostic` independently visible; it is not replaced by egress metadata.
- Headline: “已记录 native Codex 请求载荷估计（含重试）”. Display request/response directions separately and show `*_known_attempts / recorded_attempts` coverage. If combining directions, preserve unknown/partial coverage rather than silently replacing null with zero.
- Summary rows group by the whole frozen selection, including historical labels/version; one current group/member can have multiple historical rows. Do not overwrite historical addresses/names with a current-group lookup.
- `wire_bytes` and `supplier_billed_bytes` remain null: display “未记录”. This is neither actual proxy wire traffic nor supplier billing, and excludes other providers, OAuth/quota/model-listing calls, headers, handshakes, framing, TLS and retransmissions. Never label this “全流量”, “供应商用量”, or attribute the supplier's historical 40+GB to these estimates. Whole-Pod counters are not MTC attribution.
- Persistence is asynchronous and best effort. Initial observations can precede completion; updates can lag or be lost under saturation/failure. Empty data is not proof of no activity. No historical backfill is promised.
