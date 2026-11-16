# Reusable staging release acceptance

Status: framework implementation; design review and cluster acceptance pending.

The external mock reuses `benchmark-memory.ts` JSON/SSE/large-response protocol generators. Dedicated prefixed mock paths inject slow reads, HTTP 503 and disconnects. This framework is shared by release batches rather than copied per feature.

The execution topology is an exact-revision GHA-built artifact, then a run-labelled mock and load Job applied by the sole TEST deployment writer. Both run in `memeloop-token-center-test` with service-account token automount disabled. The real TEST gateway, PostgreSQL and archive services remain in the request lifecycle. Only run-scoped synthetic tenants, keys, accounts, model routes and sessions may be created. Every upstream route must resolve to this run's mock, never a paid provider. TEST credentials are referenced only through TEST Secrets and remain out of reports. No production deployment is changed by this workflow.

The mock and generator must be independently resourced so generator pressure cannot be mistaken for gateway saturation. An open-loop schedule records offered, started, completed, cancelled, failed, generator-lagged and unissued arrivals. Concurrency limits may protect the generator but must make the qualification gate fail when measured offered load is not admitted. Preserve client session IDs, tool-call IDs and follow-up tool results.

## Historical baseline evidence

The final baseline contract includes source revision, observation window and sample coverage; rolling 1-second and 60-second offered rates; overlapping start/end in-flight peak; request and response byte distributions and maxima; stream duration distribution and maximum; protocol, streaming and tool mix. Only aggregate metadata is retained. Missing dimensions remain unknown.

Current read-only evidence supplied by operations is a 48-hour, one-minute-sampled maximum `/v1/responses` response-header completion rate of approximately 1.70/s and an all-route active-request gauge maximum of 91. These are sampled lower bounds, not historical offered-load or exact concurrency peaks. Request/response bytes, stream duration, tool mix and one-second arrivals remain unknown. Zeroes from an unrelated Higress listener must not be substituted for the real nginx ingress. Current baseline validation is deliberately strict; its minimal schema will be extended to carry the full coverage contract before qualification.

## Qualification and reporting

A run covers the complete measured peak arrival window, the longest surviving stream and subsequent archive drain. Separate conservative maxima scenarios must not be described as a measured joint distribution. Ordinary phases require all expected mock results, matching hashes and tool IDs, no dropped load, exactly-once financial outcomes, no cancelled-request leaks, pool waiters/leases returning to baseline, and accepted archive debt confirmed after recovery. Report latency p50/p95/p99/max with configured mock delay separated. Without an approved SLO, compare current and candidate images under the same environment rather than inventing a latency threshold.

Fault phases isolate mock upstream errors, slow upstream reads/writes, midstream disconnect/cancellation, temporary TEST-object unavailability and slow database access through a test proxy. Do not kill shared PostgreSQL or nodes. Verify unknown completion is not replayed and restored archive/ledger effects are exactly once. Semantic/archive read APIs must return their expected successful payloads after drain; proxy HTTP 200 alone is insufficient.

Batch-specific history assertions supplied by the history owner cover credential/config/status updates without history revision fan-out, exact-once rename identity updates, financial acknowledgements during slow archive/export activity, and queue acknowledgement only after confirmed durable writes. This harness does not modify those production features.

Until complete historical metadata, real TEST lifecycle results and the approved release-batch checks are available, the release qualification is **NOT_EVALUATED**. Passing the framework's focused GHA tests is never reported as passing historical peak acceptance.
