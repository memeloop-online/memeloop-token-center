# Proxy phase diagnostics

Use the server-owned `x-mtc-request-id` returned by the gateway to correlate phase
logs with the durable request record. Failed authentication, body admission,
JSON parsing, route preparation, and failed durable admission can return an ID
without a request record: diagnostics deliberately do not make extra database
writes, including when storage is unavailable. Ingress failures before MTC
cannot produce these events; correlate those separately with ingress logs.
`gateway_entry.ingress_request_id` separately records an incoming `x-request-id`
only when it parses as a UUID; it never controls the server-owned request ID.
Use this mapping to join ingress and MTC logs without logging arbitrary headers.

`gateway_entry.route_class` distinguishes `responses` from
`responses_compact`. The latter is diagnostic recognition, **not** a supported
route: `/v1/responses/compact` continues to return 404. A normal Responses call
can be used by a client to summarize context; `request_shape.compaction_hint`
only reports the explicit existing hint and does not infer body semantics.
An absent hint is not proof that the call was not client-side compaction.

| Phase | What the elapsed time measures |
| --- | --- |
| gateway_authentication / gateway_request_body | Authorization and bounded body ingestion before the handler |
| request_preparation / route_preparation | JSON/traffic-policy work and candidate/price preparation |
| request_archive_admission | Reservation, request record and encrypted request spool transaction |
| retained_memory_admission / candidate_selection | Local memory admission and account/probe/recovery selection |
| codex_transport_attempt | One connection/send attempt until response headers or transport error; not pure TCP time on success |
| upstream_response_admission | Candidate send including bounded response admission/classification |
| buffered_response_memory / buffered_first_byte | Buffer capacity wait and first nonempty chunk after admission |
| codex_buffered_response / buffered_response | Complete buffered read and adapter processing |
| stream_first_byte / upstream_stream | First nonempty stream chunk and stream loop including downstream backpressure |
| terminal_delivery | Terminal handoff/gap acknowledgement and final downstream terminal-frame sends |
| buffered_archive_settlement / stream_terminal_settlement | Terminal archive/account settlement work |
| archive_terminal_handoff / archive_eof_drain | Wait for terminal ownership, then writer drain holding HTTP EOF |
| response_spool_begin / response_spool_append / response_spool_seal | Actual writer database operation, distinguishing acknowledged, rejected and database_error |
| response_spool_gap_write / response_spool_failed_fence | Owned gap write and failed-writer fence, including late database completion |
| stream_owner | Streaming owner through terminal reconciliation and archive EOF drain |
| gateway_response_headers | Entry-to-handler response; **not** end-to-end streaming completion |
| gateway_downstream_body | HTTP consumer polling through body EOS, error or drop; **not** a TCP/client acknowledgement |
| delivery_prepare / delivery_confirm | First billable frame's durable delivery-state transitions, including existing retries |

Phase events contain both phase `elapsed_ms` and `request_elapsed_ms` from the
gateway entry. Overlapping phases are not additive. `started` allows an in-flight
wait to be located; `not_completed` means cancellation or early failure, not
proof of upstream delivery. `returned` means an owner returned after handling
its own errors, not proof that persistence or network delivery succeeded.
First-byte observations can be after bounded protocol sniffing/prefetching.
None of these observations authorize retry, change billing, or set health.

`gateway_downstream_body` emits one summary, not per-frame logs. `bytes` counts
data returned by actual HTTP body polls; `frames` also counts forwarded trailers.
`first_poll_ms` / `last_poll_ms` and `first_data_ms` / `last_data_ms` are measured
from gateway ingress. `end_stream` means the consumer polled EOS or the final
frame with `is_end_stream`; `body_error` preserves an error without logging its
text; `dropped` means the consumer dropped an unfinished body. An already-empty
body that is never polled reports `end_stream_unpolled`, not a disconnect.
These observations preserve frame order, trailers, size hints, and the original
EOF/permit owner. They prove neither socket flush nor client consumption. Join
the request ID with validated ingress ID and ingress disconnect timing before
attributing a `200 + downstream_remote_disconnect`.

Delivery prepare/confirm phases retain safe SQLx classification in the
`proxy_delivery_database` span and report cancellation as `not_completed`.
Generic buffered reads preserve only the fixed `upstream_read_timeout` and
`upstream_request_timeout` labels; other transport errors remain
`upstream_stream`, never a provider error message.

An automatic client compaction is not identifiable from duration alone. Obtain
its client-side start/end and returned server request ID; otherwise an explicit
compaction hint or independently matched request trace is needed. A normal
`responses` request with no hint must not be relabeled by inspecting its body.
Removing an ingress body cap does not remove application safeguards: Responses
defaults to 16 MiB (configurable up to 64 MiB), request reads remain bounded to
60 seconds, and responses/SSE products have independent caps. Inspect actual
request byte counts, configured limits and 408/413 evidence before proposing
limit changes; this diagnostics change adds or changes none.

`archive_budget_acquire` separates `pool_wait_ms` (connection acquisition and
transaction begin) from `budget_wait_ms` (the singleton budget row acquisition).
It is DEBUG normally and WARN if either exceeds 250 ms; successful chunks do
not add INFO events. These are acquisition timings, **not** transaction hold
times. Join them through the enclosing request/writer span and compare the
existing admission or settlement duration. A slow lock wait identifies a waiter,
not its blocker: capture `pg_blocking_pids`, transaction age and sanitized query
classes before attributing the holder. An idle-in-transaction chunk insert alone
does not prove an object upload or downstream ACK is awaited inside PostgreSQL.

Buffered admission publishes its event only after inserting the archive, in the
same atomic transaction; the event cursor remains serialized through COMMIT.
Request and response first batches are sealed before opening the transaction,
with the existing bounded batch size. Larger bodies still seal subsequent
batches under the budget lock; this change does not remove all budget contention
or change the budget-first ordering, atomic reservation/refund, or writer EOF
ownership.

For a late `response_spool_writer` failure, first inspect correlated
`response_spool_producer` and writer outcomes: `queue_capacity`, `queue_closed`,
`terminal_sender_dropped` and `abandoned_before_*` are not database failures.
An acknowledged-false write has a `response_spool_write_rejected` event with a
fixed reason (owner/state/expiry/replay/sequence/capacity). Actual SQLx errors
retain the existing safe `error_kind` under the `response_archive_database`
span with the same request ID and operation phase, before conversion to
`AppError::Internal`. This distinguishes pool timeout, query cancellation and
other storage classes without retaining SQL messages or bound parameters.
Normal per-chunk append events are DEBUG, not INFO. One INFO writer summary
reports append attempts, acknowledged chunks/bytes, total append wait and maximum
append wait on writer exit; rejected/error appends are WARN immediately. Existing
log filtering controls detailed append output without a new diagnostic service.

The MTC 503 JSON message `no healthy upstream is currently available` is produced
after admission by `finish_unavailable` and normally has a durable error code.
It is distinct from an ingress-generated plain-text `no healthy upstream`.
Likewise, a client `error decoding response body` does not alone establish a
compression-format bug: correlate ingress content encoding/stream reset
evidence, MTC stream outcome, and the account's connect/response phases first.

Do not collect request or response bodies, raw URLs, authorization headers,
session IDs, credential material or provider error strings for this diagnosis.
The added events contain only server IDs, fixed phase/outcome labels, counters,
status and account generation. The only request-shape hint is a boolean.

No request limits, retry/timeout policy, health/reset behavior, archive ordering,
or external reverse-proxy configuration is changed by this instrumentation.
