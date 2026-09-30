# Forwarding and persistence isolation audit

Baseline: `e16b5579` on `origin/master`. Implementation and CI run from the
isolated `fix/forward-storage-isolation-20260930` checkout. The original checkout
was only read. Its dirty pricing fallback, Codex transport diagnostics and
multi-agent normalization, request/session query fields, tests, lockfile and web
changes are not included or reverted. In particular the upstream-model pricing
change in the original dirty tree still needs independent integration review.

## Actual call graph and blocking boundaries

| Stage | Calls reached by forwarding | Classification / current status |
| --- | --- | --- |
| Before body | `authenticate_gateway_before_body` / downstream credential queries | Required authentication; fail closed |
| Responses ingress | `admit_gateway_request_body_with_memory` → `RequestSpoolAdmission::capture`; `proxy_openai_responses` → `RequestSpool::read_all` | Local filesystem waits still present; source storage is used to enforce the large-body memory envelope, not just archival |
| Preparation | `proxy_with_identity_and_conversation_spool` → `pin_application_plugins`, traffic policy, authorized candidate query, route refresh/materialization | Authorization and policy evaluation remain synchronous |
| Session preference | `session_route_account_to_avoid` → DB lookup, 50 ms timeout | Optional lookup still adds persistence-dependent preparation latency |
| Capacity | Codex dispatch admission, request memory reservation, retained memory admission | Forwarding resource limits remain bounded; these are not archive queue admission |
| Billing admission (before) | `start_proxy_request_with_archive_compression` → preseal → reserve shared archive budget → usage reservation / request owner / encrypted spool / global event cursor → commit | Archive and event waits previously rejected or delayed dispatch |
| Billing admission (after) | `start_proxy_forwarding_request` → usage reservation / unique request owner → positive commit | Archive bytes and started-event cursor excluded; no dispatch on failed/unknown commit |
| Deferred request capture | `persistence::capture` → nonwaiting submission → started event → retention sanitizer → archive reservation/capture | Errors/capacity affect archive/event availability only; already admitted forwarding receives no result |
| Candidate dispatch | generation/transport revision refresh, health claim, reservation resizing on failover, native credential refresh | Required authorization/quota/ownership fences preserved |
| First billable SSE output | `delivery::send_frame` → `prepare_proxy_delivery` → `mark_proxy_delivery_started` | Required owner/ceiling CAS remains before billable delivery; no archive transaction in these methods |
| SSE frames | `ResponseArchiveProducer::append` → bounded `try_reserve` | No await; full/inactive writer abandons capture. Retention transforms still consume bounded CPU on stream task |
| Validated delivery | `UpstreamAttemptGuard::delivered_validated_output` → recovery DB writes | Still awaited after a billable send; can delay subsequent frames |
| SSE terminal/EOF | producer seal → transfer writer ownership; protocol terminal validation/delivery → drop HTTP sender | Archive writer owns no HTTP sender. Session evidence, conversation projection and finalization run after EOF |
| Buffered terminal | usage validation → conversation projection → `finish_proxy_request_with_retry` → financial CAS / settlement / analysis facts / events | Archive capture removed. Analysis and conversation still mixed with financial finalization and can delay buffered output |
| Buffered archive | After successful settlement → nonwaiting `persistence::capture` | No database ACK before delivery; scoped owner can capture a completed request |
| Worker / health | Spool upload/reaper and archive canary readiness | Object upload already asynchronous; archive readiness reports degraded without withdrawing gateway readiness |

## Invariants implemented by this patch

1. Dispatch still requires a positively acknowledged atomic quota reservation
   and unique request owner. Authentication, grants, traffic/safety policy,
   generation/transport fences, quota resizing and billing CAS are unchanged.
2. Production proxy billing admission does not call archive encryption, archive
   budget admission, spool insertion or event-cursor allocation.
3. Deferred buffered capture has at most four admitted jobs (including running
   jobs), no waiting submitters, and a 64 MiB charged envelope. Each charge covers
   three body copies, 256 bytes per scanned JSON node and 4 MiB of batch overhead.
   Over-budget input is rejected for archival before cloning/parsing a JSON tree.
   This is an allocation model, not an RSS upper bound.
4. Background capture uses a separate two-connection pool. Native stream archive
   writers have at most four owners and a separate 4 MiB memory budget. The
   existing per-stream queue is three complete 64 KiB chunks; producer/writer
   permits bound complete chunks together, plus one partial chunk.
5. Accepted archive jobs keep capacity until SQL/refund returns. Failed deferred
   refunds use the existing durable expiry recovery and do not spawn additional
   unbounded retries. No timer cancels a billing commit or replays an upstream.
6. SSE archive begin/append/seal/fence completion is not a prerequisite for first
   byte, terminal frame or HTTP EOF. Protocol validation and delivery-owner CAS
   remain prerequisites where they were previously required.
7. Persistence overload/failure is visible in fixed-label
   `memeloop_token_center_deferred_persistence_total`, jobs/bytes gauges and
   structured gap logs. Request/response locators remain `gap://` until upload
   binds verified content. EOF no longer promises durable archive visibility.

## CI evidence

Only GitHub Actions runs builds/tests. Local commands are restricted to source
review, formatting and `git diff --check`.

- `deferred_persistence_normal_failure_and_saturation_preserve_forwarding`:
  real gateway, buffered and streaming output, SQL archive failure, all four
  background slots occupied, all persistence connections held, and a closed
  persistence pool; one upstream invocation and one settlement.
- `pending_response_archive_begin_does_not_hold_first_byte_or_eof`: archive begin
  latch remains closed until a complete HTTP body is received and the forwarding
  memory budget returns to zero; archive memory remains independently charged.
- `postgres_archive_budget_lock_does_not_block_dispatch_or_buffered_delivery`:
  a real PostgreSQL transaction holds the shared budget row while dispatch and
  the entire buffered response complete.
- Queue unit contracts cover job/byte saturation, rejected futures never running,
  and capacity recovery after a failed job.
- Existing billing-admission failure, authorization, quota, duplicate settlement,
  stream validation and memory acceptance suites remain required release gates.

See PR #423 for the current head and Actions conclusions. No unrun test is a pass.

## Unresolved release risks

This patch does **not** establish the universal claim that all persistence or
analysis latency is independent of forwarding latency. It establishes explicit
archive scheduling boundaries, subject to the remaining coupling below.

- Filesystem request spooling is still in Responses preparation. Removing it
  safely requires an independently bounded source representation and revised
  large-body memory acceptance, including the existing 25.8 MiB fixture.
- `settle_token_usage_in_transaction` itself takes the statistics projection lock;
  financial backfill/reconciliation uses the same consistency boundary. Moving
  that lock or making settlement lossy would violate the billing requirement.
  Financial facts and derived analysis need a separate recoverable projection
  protocol before this path can be declared isolated.
- Optional session lookups and upstream health observation still wait for SQL.
  They need bounded cached/owned publication semantics without weakening hard
  quota or credential-generation fences.
- SQLite has one writer lock even across separate pools. PostgreSQL still shares
  server CPU/I/O, table-level locks, schema and physical availability. A separate
  pool isolates connection starvation, not these resources.
- Background jobs and stream finalizers can still hold request-associated state.
  Archive work is bounded, but slow financial/analysis finalization can consume
  forwarding lifecycle permits. Removing that coupling requires a durable billing
  handoff, not dropping a full queue or unconditionally allowing new requests.
- Crash/shutdown can lose accepted but uncommitted archive bytes or a started
  event; the durable request owner and reservation survive. Archival is best effort,
  and this patch does not provide lossless archive guarantees.
- Async started events must not appear after finished events. Publication skips
  already completed records under the shared event cursor ordering.
- Archival regression assertions must await independent archive progress rather
  than infer durability from receiving a response. CI must verify retention,
  chunk integrity, exactly-once financial side effects and archive worker races.

Keep the PR draft until the CI results and remaining mandatory scope are resolved.
