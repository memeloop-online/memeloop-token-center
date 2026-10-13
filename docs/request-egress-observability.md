# Request egress observations

Schema 124 records native Codex transport attempts. This is a separate source change from the schema-123 release. It does not backfill earlier requests or attribute a supplier's historical traffic total.

## Operator API

The existing authenticated `GET /internal/v1/requests/{request_id}` returns `egress_attempts`. Tenant ownership is checked using the existing request-detail authorization before reading observations. An empty array means **not recorded**. Self-service responses omit the operator field.

The Rust response types are `src/db/routing/egress.rs::{RequestEgressAttempt, SelectedEgress, RequestEgressSummary}`. Each attempt has:

- `attempt_id`: unique for each actual send, including same-account HTTP400 replay. `outbound_attempt` and `connect_attempt` retain existing routing coordinates and may repeat; they are not unique keys.
- `selected`: `kind`, `group_id`, `group_version`, `group_name`, `member_id`, `member_name`, `scheme`, `host`, `port`. Managed metadata is frozen while selecting the credential, before send, and follows request-local member overrides. Unmanaged groups retain the selected address and member index without inventing managed IDs or labels.
- `transport`: existing cached-client instance, member index/count, selection epoch/version, candidate rank and request-local selection evidence. A client instance is not a connection ID. Optional `connection_binding` reuses the H2 observer's process-local connection and stream IDs; disabled/unavailable bindings are null.
- `request_payload_bytes`, `response_payload_bytes`: application payload estimates. Null means unrecorded; zero means observed zero. `completed` means the attempt terminated, including error or cancellation. `response_body_complete` requires observing the response stream's EOF; early drop remains incomplete.
- `final_response`: the existing candidate loop selected this upstream response after failover decisions. It does not mean success or successful delivery to the downstream. Attempts consumed internally by HTTP400 classification, discarded candidates, and transport failures are not final responses.
- `measurement_basis`: `application_payload_estimate`.

Protocol, host and port are visible to authorized operators. URL username/password, path, query and fragment are never persisted in the egress snapshot. Existing managed-group responses also expose host/port; current configuration and historical attempt selection remain separate facts.

`GET /internal/v1/requests/egress-summary?start=<unix-ms>&end=<unix-ms>&tenant_external_id=<optional>` uses existing `requests:read` authorization and tenant restrictions. The half-open window applies to attempt start time. Results group by the frozen selected egress, including historical labels/version. All recorded attempts, including retries and partial failures, contribute to the observed payload totals; `final_response_attempts` is separately reported. Known-byte attempt counts expose missing observations. A scoped query uses request locators to enforce tenant ownership. Counts are observations, not a completeness guarantee; empty results are not proof of zero traffic.

## Measurement and persistence

Request bytes come from existing transport body polls, not the prepared body length. Response bytes are counted at the upstream byte stream before SSE parsing, admission prefetch and response rewriting, so prefetched bytes are not counted twice. Totals can include decoded/decompressed payload. They exclude HTTP headers, CONNECT/SOCKS handshakes, H2 frames, TLS, TCP retransmission, idle connection traffic and provider-side billing rules. `wire_bytes` and `supplier_billed_bytes` remain null in summaries. Whole-Pod counters cannot be substituted for these MTC observations.

Records use the existing nonblocking routing-persistence queue and independent persistence database pool. Each attempt coalesces pending snapshots behind its admitted job instead of admitting a separate job for every begin, terminal and final-selection update. Observation admission leaves one existing job slot available for health persistence; the existing total job/byte budgets are unchanged. Begin and terminal/selection snapshots are idempotent; cumulative counters use monotonic updates and immutable identity conflicts are rejected. SQL is never awaited by forwarding, and queue saturation or storage failure only increments existing persistence gap metrics. There is no new forwarding deadline, retry policy, concurrency limit, dedicated worker or persistence retry loop. Queue loss, process crashes and active attempts can leave missing/partial records.

The drop-owned recorder is armed only when the send future is polled, after successful request construction and body/binding observer attachment, immediately before execute. Unpolled sends, already-expired deadlines and request-build failures produce no actual-send record. Cancellation, stream errors and deadline cancellation after arming submit the consumed prefix. A successful response transfers the recorder to its byte stream and carries a selection handle to the existing candidate-selection point. When integrating native H2 recovery, retain `AttemptRecorder::begin` at this execute boundary inside each actual iteration so every physical replay receives a fresh ID; do not move the guard outside the recovery loop.

The aggregate query runs only in the control-plane read path, independently of forwarding. It covers recorded native Codex generation sends, not other providers, quota refresh, model listing, every Pod socket, or all uses of a proxy account. Retention follows a separate operational decision; this change does not delete historical request observations automatically.

## Validation

Adjacent SQLite/PostgreSQL projection tests cover duplicate/out-of-order updates, immutable snapshots, repeated routing coordinates, retry sums, nullable coverage and migration replay. Gateway tests cover real body consumption across HTTP400 replay and successful forwarding with a failed or saturated sink. Sender tests cover unpolled/expired/build-failed sends, cancellation before headers, observation before response completion, stream ownership and coalesced terminal/selection updates. Queue tests separately verify health headroom without waiting. Managed-group tests cover visible non-secret addresses and frozen member identity after rotation. Run through the existing GHA CI; no local build/test/install is required or used for this change.
