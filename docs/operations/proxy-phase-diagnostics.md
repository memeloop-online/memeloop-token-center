# Proxy phase diagnostics

Use the server-owned `x-request-id` returned by the gateway to correlate phase
logs with the durable request record. Failed authentication, body admission,
JSON parsing, route preparation, and failed durable admission can return an ID
without a request record: diagnostics deliberately do not make extra database
writes, including when storage is unavailable. Ingress failures before MTC
cannot produce these events; correlate those separately with ingress logs.

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
| stream_owner | Streaming owner through terminal reconciliation and archive EOF drain |
| gateway_response_headers | Entry-to-handler response; **not** end-to-end streaming completion |

Phase events contain both phase `elapsed_ms` and `request_elapsed_ms` from the
gateway entry. Overlapping phases are not additive. `started` allows an in-flight
wait to be located; `not_completed` means cancellation or early failure, not
proof of upstream delivery. `returned` means an owner returned after handling
its own errors, not proof that persistence or network delivery succeeded.
First-byte observations can be after bounded protocol sniffing/prefetching.
None of these observations authorize retry, change billing, or set health.

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
