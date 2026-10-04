# Forwarding and persistence isolation audit

Integration baseline: `5e2cef492189ffd76558bf1f541a8f5e430ea24b` on
`origin/master`. Continued the existing conflicted integration in
`mtc-forward-storage-completion-20261003`, preserving its staged and dirty work
and merging the existing PR #423 head `608c21ff` without replaying it. Master
cancellation settlement (#451), typed transport errors (#434), and asynchronous
projection work remain included. The related terminal-cause worktree is untouched.
Historical PR #423 CI evidence predates this integration and is not a pass for
the integrated head. Validation is GitHub Actions only; no production changes.

## Actual call graph and blocking boundaries

| Stage | Calls reached by forwarding | Classification / current status |
| --- | --- | --- |
| Before body | `authenticate_gateway_before_body` / downstream credential queries | Required authentication; fail closed |
| Responses ingress | `admit_gateway_request_body_with_memory` → `RequestSpoolAdmission::capture`; `proxy_openai_responses` → `RequestSpool::read_all` | Local filesystem waits still present; source storage is used to enforce the large-body memory envelope, not just archival |
| Preparation | `proxy_with_identity_and_conversation_spool` → `pin_application_plugins`, traffic policy, authorized candidate query, route refresh/materialization | Authorization and policy evaluation remain synchronous |
| Session preference | `session_route_account_to_avoid` → bounded cache, nonwaiting refresh admission | No SQL awaited; a miss temporarily supplies no optional avoidance preference |
| Capacity | Codex dispatch admission, request memory reservation, retained memory admission | Forwarding resource limits remain bounded; these are not archive queue admission |
| Billing admission (before) | `start_proxy_request_with_archive_compression` → preseal → reserve shared archive budget → usage reservation / request owner / encrypted spool / global event cursor → commit | Archive and event waits previously rejected or delayed dispatch |
| Billing admission (after) | `start_proxy_forwarding_request` → usage reservation / unique request owner → positive commit | Archive bytes and started-event cursor excluded; no dispatch on failed/unknown commit |
| Deferred request capture | `start_proxy_request_with_deferred_archive` → gateway persistence nonwaiting submission → started event → retention sanitizer → archive reservation/capture | Errors/capacity affect archive/event availability only; already admitted forwarding receives no result |
| Candidate dispatch | generation/transport revision refresh, health claim, reservation resizing on failover, native credential refresh | Required authorization/quota/ownership fences preserved |
| First billable SSE output | `delivery::send_frame` → `prepare_proxy_delivery` → `mark_proxy_delivery_started` | Required owner/ceiling CAS remains before billable delivery; no archive transaction in these methods |
| SSE frames | `ResponseArchiveProducer::append` → bounded `try_reserve` | No await; full/inactive writer abandons capture. Retention transforms still consume bounded CPU on stream task |
| Validated delivery | `UpstreamAttemptGuard::delivered_validated_output` → bounded publication admission | No SQL awaited after a billable send; delivery-owner CAS remains mandatory |
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
3. Deferred buffered response capture has at most four admitted jobs (including running
   jobs), no waiting submitters, and a 64 MiB charged envelope. Each charge covers
   three body copies, 256 bytes per scanned JSON node and 4 MiB of batch overhead.
   Over-budget input is rejected for archival before cloning/parsing a JSON tree.
   This is an allocation model, not an RSS upper bound.
   Request capture preserves master's separate queue: at most 16 jobs, a 32 MiB
   charged envelope using the same allocation model, one writer, and a two-second
   optional-job deadline. The deadline never wraps mandatory billing admission.
4. Response capture uses a separate two-connection pool with lazy connection
   establishment, so its availability is not a new startup gate. Native stream archive
   writers have at most four owners and a separate 4 MiB memory budget. The
   existing per-stream queue is three complete 64 KiB chunks; producer/writer
   permits bound complete chunks together, plus one partial chunk.
   Request capture uses master's separate lazy one-connection pool with bounded
   SQL timeouts. These are two independently bounded queues, not one shared cap.
5. Accepted response archive jobs keep capacity until SQL/refund returns. Failed
   deferred refunds use existing durable expiry recovery without unbounded retries.
   Request jobs retain master's timeout and durable refund recovery semantics.
   No timer cancels a billing commit or replays an upstream.
6. SSE archive begin/append/seal/fence completion is not a prerequisite for first
   byte, terminal frame or HTTP EOF. Protocol validation and delivery-owner CAS
   remain prerequisites where they were previously required.
7. Persistence overload/failure is visible in fixed-label
   `memeloop_token_center_deferred_persistence_total`, jobs/bytes gauges and
   structured gap logs. Request/response locators remain `gap://` until upload
   binds verified content. EOF no longer promises durable archive visibility.
   The independent request queue exposes
   `memeloop_token_center_request_persistence_total` with accepted, capacity,
   failed and retention-limit outcomes, plus its own jobs/bytes gauges. Queue
   byte/slot rejection and shared spool-budget rejection count as capacity;
   SQL errors and optional-job timeouts count as failures.

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
- `postgres_archive_budget_lock_does_not_block_upstream_dispatch` and
  `postgres_request_archive_table_lock_does_not_block_response_body`:
  a real PostgreSQL transaction holds the shared budget row while dispatch and
  the entire buffered response complete.
- Queue unit contracts cover job/byte saturation, rejected futures never running,
  and capacity recovery after a failed job.
- Existing billing-admission failure, authorization, quota, duplicate settlement,
  stream validation and memory acceptance suites remain required release gates.

See PR #423 for the current head and Actions conclusions. No unrun test is a pass.

Integration run `37131774170` passed Rust, security, API contracts, migrations,
packaging, binary production and both image assembly checks. Web failed a tooltip
description association assertion. The independent RSS gate passed its first
64 MiB response phase and then failed during the internal drain in
`four-concurrent-16MiB-known-length-inputs`: request queue capacity gaps were not
represented by the response-only counter. Its observed peak was 295.71 MiB, below
the unchanged 448 MiB ceiling. A 16 MiB request's three-copy charge plus overhead
exceeds the unchanged 32 MiB request archive envelope and legitimately omits
archival. This is distinct from failing mandatory request admission.

The corrected gate drains both queues, rechecks the durable spool budget after
capture drains, and requires request gaps to have request-capacity evidence and
response gaps to have response-capacity evidence independently. Neither queue's
failures are permitted on healthy storage. It records the current phase before
execution so an internal drain failure retains its case name. Concurrency, memory
ceilings, exact byte/hash checks, settlement and recovery assertions are retained.

The Web correction resolves the tooltip from the current trigger's
`aria-describedby` tokens before waiting for visibility and checking its content.
The old global visible-text lookup could capture a previous focused trigger's
tooltip with overlapping candidate text during Fluent's focus delay, then wait
forever for that other tooltip's ID on the current trigger. The description
association, visible content, keyboard/touch/Escape, viewport and no-write
assertions remain; no production tooltip behavior or timeout is changed.

Terminal-cause integration must preserve PR #455's separate typed
`FinishProxyRequest.terminal_cause: Option<RequestTerminalCause>` through archive
fallback, including `..input`, into the same winning `completed_at IS NULL` CAS
(bind 19). Do not derive it from the public error code or earlier retry evidence.
Migration 0114 remains reserved for PR #422; 0115 belongs to terminal cause.

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
- Optional session lookup and health publication have bounded cached/owned
  publication semantics in the continuation below; new-head GHA acceptance is
  required before considering this specific blocker closed.
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

## Optional routing publication slice (October 4)

This continuation reuses the existing `Persistence` nonwaiting semaphore admission
implementation in an independent four-job routing lane. It does not change archive
queue limits or forwarding concurrency. Routing SQL uses an independent lazy
two-connection pool. Session refresh and streaming terminal evidence have 250 ms
deadlines; optional terminal health jobs have a two-second deadline. These timers
never wrap reservation, delivery-owner or credential CAS. Queue rejection does not
create a waiting submitter or spawn a task. The metrics endpoint exposes
`memeloop_token_center_routing_persistence_total` with accepted, capacity and failed
outcomes, and routing jobs/bytes gauges. A failed job can record multiple failed
publication operations; the failed counter is not an exact count of lost requests.

Session preferences use the existing bounded deque-cache pattern: at most 256
fixed-size hashed identities, one-second TTL, negative/miss coalescing, and
nonwaiting cache-lock admission. Tenant, principal, key, credential generation,
session, model and protocol are all part of the identity. Refresh inputs have a
4 KiB combined string limit. Expired values are not reused; explicit policy hints
still override avoidance, and authorized candidate selection remains mandatory.
Local streaming terminal evidence updates the cache before EOF. Cross-replica
visibility is eventual and best effort; EOF no longer promises durable session
evidence. A concurrent refresh cannot overwrite a newer local cache observation.

Delivery recovery and terminal health are ordered per attempt by an owned result
channel. Existing generation/revision/epoch/lease SQL predicates remain intact.
Optional terminal jobs own their probe heartbeat and shared-probe permit only;
they own no downstream sender, forwarding memory reservation, dispatch permit or
request lifecycle permit. Rejection/timeout drops heartbeat ownership, leaving
durable probe expiry as recovery. Authentication and hard-quota failure transitions
remain synchronous, as does the existing committed-media completion contract.
Streaming session publication no longer waits before settlement or falls back to
session SQL in that settlement when optional admission fails.

GHA runs `api::proxy::tests::routing_persistence::` explicitly, followed by the
full existing suite. The regression exercises buffered and streaming gateways,
held optional-pool connections, a closed optional pool, saturated publication
capacity, exact once financial effects and upstream invocation, and restored
lifecycle capacity before releasing optional SQL. Single-poll health tests reject
any SQL await on the forwarding caller. Cache tests cover scoped identities and
authoritative policy hints. Local validation is formatting/source review only;
this slice has no CI pass until the new head finishes Actions.

Remaining release blockers still include filesystem request spooling, financial
statistics projection locking, buffered session/conversation work inside financial
finalization, physical database contention and durable financial lifecycle handoff.
Those mandatory finalization paths may still retain lifecycle permits. This slice
removes the optional routing publication hold; it does not claim global isolation.
PR #455's `terminal_cause` interface and migration ownership remain unchanged.

## PR #457 comparison and single integration line

PR #423 is the only integration line. PR #457 at `a8ab8772` remains open for the
parent's review; its merge base is `b116e25a`, not the later green archive baseline
`37f861dd`. No whole-branch merge or second observation queue has been imported.
The four-file work in progress before this comparison was preserved at
`/tmp/pr423-four-file-wip-20261004.patch` and incorporated, not discarded.

| Boundary from #457 | Decision on #423 | Acceptance |
| --- | --- | --- |
| Cache refresh version token and local terminal ordering | Adapted explicit UUID version, replacing timestamp identity; retain timestamp/request UUID terminal order | `local_session_versions_fence_refresh_invalidation_and_older_terminals` |
| Buffered winning settlement invalidates cached preference and pending refresh | Adapted; only `Finished`, never failed or duplicate settlement | Existing buffered settlement gates plus the version/invalidation regression |
| Tenant/principal/key/session/model/protocol scope | Retain #423 scope including credential generation | `session_preferences_are_scoped_cached_and_explicit_hints_win_without_sql` |
| Delivery account generation, transport revision, lease/epoch and probe validity | Retain existing fenced SQL; adapt queued-stale-evidence regression | `queued_delivery_recovery_respects_epoch_generation_revision_and_probe_expiry` |
| Accepted delivery recovery is not cancelled by an application timer | Adapted: admitted delivery SQL retains its slot until SQL returns; database deadlines still apply | Pool-starvation and cancellation regressions |
| Cancellation during recovery acknowledgement preserves guard ownership | Adapted borrow of receiver before mandatory completion/abandonment takes state | `cancelled_lease_abandonment_retains_recovery_order_and_bounded_cleanup` |
| Saturated delivery queue converges through terminal owner | Intentionally different: #423 terminal publication is also optional and bounded; rejection stops heartbeat and relies on fenced lease expiry/new probe, with visible gaps | `saturated_terminal_publication_converges_by_fenced_probe_expiry` |
| Pool starvation/closed pool/queue saturation must not delay output | Adapted to #423 queue; also require lifecycle permit recovery and exact once settlement | `optional_routing_sql_never_holds_forwarding_or_lifecycle_capacity` |
| Synchronous hard-quota fence | Retained; optional hook/diagnostic publication uses bounded admission | `mandatory_quota_fence_does_not_wait_for_optional_observe` |
| Separate 16-slot delivery queue and 1024-entry cache | Not imported: retain one four-job routing lane and 256-entry cache on #423 | Existing queue limit plus new saturation tests |

The delivery SQL no longer has the earlier 250 ms application timer. Optional
terminal publication still has a two-second deadline, and waits for an admitted
delivery acknowledgement before issuing terminal SQL. Timeout or rejection never
asserts health recovery. Mandatory completion and explicit lease abandonment keep
the receiver in the guard while awaiting it, so cancellation transfers cleanup
without racing a pending delivery acknowledgement.

CI run `37170299514` at `ee940218` failed clippy because the compatibility wrapper
`latest_session_transport_route_to_avoid` had only test callers but was compiled
in production (`src/db/requests/session_routing.rs:40`, `dead_code` under
`-D warnings`). The precise fix restricts that wrapper to `#[cfg(test)]`; production
continues using the identity-scoped method. No lint suppression or gate removal.
The focused regressions had not run at that failure. All rows above remain pending
new-head GHA acceptance; the comparison is not permission to close #457 or release
#423. No #455 worktree, migration, typed terminal-cause field, or winning settlement
CAS has been replaced during this consolidation.

The #457 Rust failure in run `37134521146` was
`queued_delivery_recovery_cannot_clear_a_newer_health_epoch`: expected
`last_failure_kind = authentication`, observed an empty string. Source review
shows its setup called the generic failure writer during an active probe lease
and discarded the returned boolean; that writer intentionally rejects updates
while the current probe owns the lease. The adapted
`queued_recovery_cannot_overwrite_acknowledged_authentication_failure` uses the
owned probe-token/revision CAS and requires `true` before releasing delayed
delivery recovery. This tests an actually acknowledged newer failure rather than
assuming the fixture created one. #457 itself is unchanged and remains open.

## Latest CI handoff (October 4)

At `a1793142845c`, run `37171578910`, Rust job `111345474211`, passed
Clippy with `-D warnings` and the focused routing-persistence regressions.
The full library suite reported 1510 passed, seven failed and three ignored.
The seven failures inspected optional health publication before its queue had
finished: invalid image body, streaming Codex invalid usage, transient Codex 400,
Codex retry followed by 429/5xx, and the three Wasm group-routing recovery,
cooldown and invalid-wire cases. Their assertions now await the existing bounded
test drain at the health observation point. The streaming case first retains its
lifecycle-completion barrier so the terminal job has been submitted before drain.
Failure kinds, exact Wasm cooldowns, no-replay and financial assertions are
unchanged; no production concurrency, queue, cache or CAS implementation changes.

The preserved authentication regression above is included in the same handoff.
New-head acceptance remains pending GitHub Actions; no local builds or tests were
run. #457 remains open and unmerged, and #423 remains the sole integration line.
