# Upstream concurrency observations

`GET /internal/v1/upstream-concurrency?tenant_external_id=…` requires both `providers:read` and `requests:read`. Tenant service credentials remain scoped to their tenant. Global credentials can omit the selector. Responses use `Cache-Control: no-store`.

The control process asynchronously samples existing shared database request-owner leases, generation states and egress-attempt observations every five seconds. Reading this API returns the cache. Request admission does not query or lock this collector. Each control process retains up to 15 minutes of history; restart or another control replica can change the available history.

| Field | Meaning |
| --- | --- |
| `contract_version` | `upstream_concurrency_v1` |
| `observed_at`, `last_attempt_at`, `last_error_at`, `stale` | Sampling freshness and failure gaps; Unix milliseconds. `current` is null after two intervals without a successful sample. |
| `series[].dimension`, `identity` | `upstream_account` uses account ID, `network_proxy` uses a digest of the sanitized actual scheme/host/port. Shared endpoints aggregate across group-member names and accounts within the selected tenant. |
| `series[].measurement_basis` | Live request-owner lease, individual generation lifecycle state, or live owner with unfinished recorded egress attempt. These must be displayed separately. |
| `series[].current`, `sampled_peak`, `points` | Latest observed count, peak among retained samples, and timestamped counts. Peaks are sampled lower bounds, not exact subsecond wire-concurrency maxima. |
| `series[].sample_ids`, `samples[]` | Bounded attribution examples and current source records: request/job ID, account, stage/state, lease expiry, selected egress and actual egress-attempt account/ID. |
| `samples[].lease_state`, `egress_state` | Explicit live/expired/missing lease and unfinished/completed/invalid/missing egress evidence. Missing proxy metadata is never a direct-proxy observation. |
| `metadata_gaps` | Current missing account, missing egress and invalid egress sample counts. |
| `samples_truncated`, `points[].truncated` | Observation bounds were reached; counts/peaks can undercount. Bounds limit observability memory, not customer concurrency. |
| `recommendation`, `application_mode` | Null and `manual_only`. This API does not choose, apply or enforce a concurrency limit. |

Request-owner `state` reports the existing recorded in-flight stage: `upstream_response_headers`, `upstream_response_body` or `response_delivery`. Missing or unrecognized phases remain `unknown`. Rows with a recorded operational terminal are excluded even if financial settlement is pending. Request owner leases include preparation, streaming and delivery; they are distinct from simultaneous upstream HTTP exchanges. Expired/missing request leases are excluded even if their request row remains unresolved. Generation running/submitting/cancelling/preparing/queued states remain separate and expose their lease metadata; they do not imply a live network exchange. Every account in the sampled inventory gets a zero live-request series when idle. Proxy series describe actual observed endpoints; configured but unobserved proxies have no fabricated zero series.

Codex, generic HTTP and component-provider requests asynchronously persist sanitized egress-attempt evidence using the actual selected transport proxy. A direct no-proxy transport is explicitly recorded as direct; absent observations remain missing metadata. Body counters measure consumed request bytes and original upstream response bytes, before protocol translation, and remain application estimates rather than wire acknowledgements. An unfinished event may lag its transport through asynchronous persistence. Do not equate these proxy samples with connection/stream IDs or provider-side concurrency. Independent network-proxy probe health and independent upstream-service probe health are separate systems and neither is used to invent concurrency observations.

The existing Prometheus `memeloop_token_center_upstream_active_requests` remains the process-local RAII count of HTTP exchanges by bounded provider/operation labels. It holds response-body ownership and has no account/proxy identity. This API adds account/proxy operational attribution without increasing Prometheus label cardinality.
