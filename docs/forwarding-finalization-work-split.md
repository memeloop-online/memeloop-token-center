# PR #423: derived publication and durable finalization work split

The #423 allocation below is historical. PR #469 owns the complete A1 database,
maintenance, worker/module/migration registration and test integration, including
the prune driver. There is no separate B owner for this iteration. A2 permit
handoff and body spooling remain outside this change.

## Integrated starting point

`8fcb174ac320501603d16e8eb25f80f3913a6f6e` merges master
`07b9220c18c59c5cc125caaa2478635f2e2e8765` into the previously verified
`1f90e5d16eef9115598884c3d40d00c71b65a940`.
It includes #455, #458 and #459. Baseline Actions `37174865435` passed;
the newer integration requires its own Actions result.

Preserve `FinishProxyRequest.terminal_cause: Option<RequestTerminalCause>`,
`input.terminal_cause`, `terminal_cause_code = $19`, its corresponding bind,
the winning `completed_at IS NULL` predicate, and archive fallback `..input`.
Migration 0115 and its SQLite/Postgres/history tests remain intact. Preserve
0116 transport proxy groups and their migration/routing tests. Neither numbered
migration may be repurposed. Allocate new migration numbers only after checking
the latest integration head and other workers' reservations.

## Confirmed coupling, not just queue placement

In `src/db/requests/lifecycle.rs`, the terminal transaction acquires
`lock_request_stats_projection_writer_in_transaction` before the request-owner
CAS. `record_request_finished_with_basis_and_metering_in_transaction` acquires
it again, updates the authoritative terminal row, writes facts and sometimes
aggregates, then allocates the global request-event cursor before commit.
`settle_token_usage_with_explicit_charge` in `requests/settlement.rs` also takes
the projection lock. Removing only the first call does not isolate settlement.

Prepaid conversation observation still performs session/conversation projection
inside that transaction. The metered path already has durable conversation and
usage outboxes, but its terminal fact and event publication remain coupled.
Existing metered projectors require the fact to exist; changing its timing
without changing that dependency creates a retry or missing-projection bug.

The shared projection lock excludes maintenance rebuilds. Its removal must be
paired with an exactly-once replay/rebuild protocol, not a deleted safety fence.
`scripts/maintenance/reconcile-observability-day.sql` rebuilds from source rows;
`failed_request_cost_backfill.rs` also participates in the fence. A source row
included by rebuild must not later be counted again by a pending projector.

In `src/api/proxy/streaming.rs`, the outer task owns both lifecycle and dispatch
permits through `finalize_streaming_lifecycle` and even deadline reconciliation.
The sender is dropped before this work, but the permits are dropped afterward.
EOF therefore does not release forwarding capacity when settlement is slow.
`release_orphaned_reservations` and `expire_proxy_lifecycle_deadline` currently
own recovery paths that must recognize any new durable finalization owner.

## Exclusive write sets

| Owner | Exclusive files | Responsibility |
| --- | --- | --- |
| Database worker A | `src/db/requests/lifecycle.rs`, `src/db/requests/settlement.rs`, `src/db/requests/conversations.rs`, `src/db/requests/metered_projection.rs` | Financial CAS/outbox boundary; all nested projection-lock calls; conversation and metered projection dependency; orphan/deadline fencing |
| Database worker A | New `src/db/requests/terminal_projection.rs`, new `src/db/requests/finalization.rs` | Durable derived-work receipts and durable finalization enqueue/claim/ack APIs |
| Database worker A | `src/db/projection_lock.rs`, `src/db/failed_request_cost_backfill.rs`, `scripts/maintenance/reconcile-observability-day.sql` | Projection/rebuild protocol; no direct removal of the rebuild fence |
| Database worker A | `src/db/tests/proxy_lifecycle.rs`, `src/db/tests/event_cursor.rs`, new `src/db/tests/terminal_projection.rs`, new `src/db/tests/finalization.rs`, `tests/proxy_atomic_postgres.rs` | Database, crash/replay and lock-isolation acceptance |
| #423 integrator B | `src/api/proxy.rs`, `src/api/proxy/lifecycle.rs`, `src/api/proxy/lifecycle/cancellation.rs`, `src/api/proxy/streaming.rs`, `src/api/proxy/streaming/lifecycle.rs`, `src/proxy_lifecycle.rs` | Build terminal evidence, durable handoff, release forwarding ownership at the proven boundary, preserve cancellation |
| #423 integrator B | `src/api/proxy/tests/persistence_isolation.rs`, `src/api/proxy/tests/codex_dispatch.rs`, `src/api/proxy/tests/cancellation.rs`, `src/api/proxy/tests/sse_delivery.rs`, new `src/api/proxy/tests/finalization_handoff.rs` | Real forwarding and capacity-release acceptance |
| #423 integrator B | `src/worker.rs`, `src/lib.rs`, `src/config.rs`, `src/metrics.rs`, `src/db/mod.rs`, `src/db/requests/mod.rs`, `src/db/tests/mod.rs`, `src/api/proxy/tests.rs`, `src/db/migrations/mod.rs`, `.github/workflows/ci.yml`, new migrations and these docs | Shared wiring, migration numbering, bounded scheduling, metrics and Actions |

Worker A supplies migration SQL and export/worker-wiring requirements with its
patch, but does not edit the integrator's files. A does not edit API/proxy files;
B does not edit A's database files. Requests to change an owned file go through
its owner. Other existing worktrees and #457 are not integration targets.
Generation-job implementation and transport-proxy-group files are read-only in
this slice; if shared helpers require changing their contracts, expand ownership
explicitly before editing, rather than silently removing their locks.

## Slice A1: derived publication leaves financial transactions

The winning financial transaction retains tenant/key/request/reservation checks,
delivery authority, normalized usage, pricing, reservation/ledger/balance CAS,
the terminal row and typed cause. It inserts a request-keyed durable derived-work
record in the same commit. It must not acquire a statistics-rebuild or event-cursor
lock, update aggregate/session hotspot rows, or wait for a projector.
Prepaid budget and account checks remain synchronous and authoritative.

Use the existing durable projection/outbox pattern, not the lossy optional
`routing_persistence` lane. Persist enough immutable terminal data to survive
source retention and reconstruction; no raw credential or unbounded body is an
outbox payload. Repeated enqueue for the same request must verify the same owner
and evidence rather than replace the winner.

Proposed database API boundary: bounded `claim_terminal_projection_tasks(owner,
limit)` and `project_claimed_terminal_projection_task(owner, request_id)`.
Claims use expiring leases and PostgreSQL `SKIP LOCKED`; projection and durable
acknowledgement commit together. Facts, aggregates, conversation reclassification
and metered-usage projection must have one agreed idempotency protocol. Preserve
account lifetime usage and prepaid budget semantics; do not label these all as
disposable statistics.

Event allocation happens in the projector transaction, retaining the existing
global committed cursor order. Do not substitute sequence allocation outside
commit ordering. A delayed started event must never appear after finished; the
started publisher and finished projector must share an ordering protocol.
Event payload and projection acknowledgement must survive retry atomically.

Rebuild/backfill and projection receipts need an explicit common snapshot/replay
boundary. Keep the exclusive fence among derived writers; allow new financial
commits to create pending work while it is held. Rebuild inclusion and projector
acknowledgement must agree for tasks pending before, during and after the snapshot.
Do not use a maximum UUID/timestamp as proof that all earlier commits were seen.

## Slice A2/B: durable finalization releases forwarding ownership

A2 supplies the database ownership API; B supplies the request/worker integration.
This can be reviewed separately from A1, but do not release permits before the
database ownership contract exists.

Proposed API boundary: enqueue immutable finalization evidence under the existing
request/reservation identity, returning either a committed durable owner or an
already-finished result; then bounded claim and fenced finalize/ack operations.
Evidence includes usage basis/validated usage, authoritative delivery state,
selected/dispatched attribution, original typed terminal cause, response locator,
timings and references needed for deferred conversation work. Keep request-specific
and pricing-generation checks; never reclassify a prior retry as the final cause.

Only positive handoff commit or known terminal completion allows forwarding
permit release. An unknown commit requires ownership reconciliation; it is not
proof to dispatch again, charge zero, discard evidence or release a reservation.
Cancellation, orphan cleanup and deadline cleanup must consult the same durable
owner. A leased retry must not overwrite a winner or steal newer ownership.

Use a fixed bounded worker batch (existing projection batches are capped at 32),
bounded resident envelope sizes and an explicit durable backlog/admission policy.
Reserve finalization capacity before irreversible upstream work; define release
for never-dispatched requests and completed/reconciled tasks. When capacity cannot
be reserved, reject before dispatch. Do not reduce configured forwarding
concurrency, accept an unbounded database backlog, or create one waiting task per
pending request. Completed HTTP streams must not retain body buffers or senders in
the finalization worker. The existing optional cache/health lane is not settlement
storage and remains unchanged.

## Actions-only acceptance required for each implementation

1. Hold the PostgreSQL exclusive request-stats rebuild lock, then complete prepaid
   and metered requests. Authoritative terminal/ledger state must commit before
   release; derived work must remain pending and converge exactly once afterward.
   Keep the existing 1024-request concurrency tests at their present sizes.
2. Hold the event-cursor advisory lock through settlement. Commit financial state
   first, then release it and verify one finished event, reconnect-safe cursor
   ordering and no started-after-finished inversion.
3. Crash/restart at financial commit, derived commit and acknowledgement boundaries;
   run multiple claimers, expire leases, retry stale owners, and race rebuild with
   pending work. Verify exact fact/aggregate/ledger counts, including metered and
   conversation projectors in either order and financial backfill.
4. For buffered and streaming traffic, pause the real financial finalizer after
   durable handoff. Assert response completion and restored forwarding/dispatch
   capacity while paused; assert the durable reservation remains authoritative.
   Restart the finalizer and require exactly one charge with original usage and
   terminal cause. Gate placement must exercise settlement, not optional health.
5. Fill finalization capacity and verify pre-dispatch rejection, bounded tasks and
   bytes, no accepted evidence loss, no extra upstream POST and no reduced test
   concurrency. Unknown commit, EOF/cancellation, stale lease and deadline races
   must converge without double charge or zero-charge overwrite.
6. Retain Clippy `-D warnings`, full existing Actions, 0115 terminal cause tests,
   0116 migration/proxy-group tests, typed transport provenance, memory gates and
   real gateway plugin checks. Local compilation and automated tests are forbidden.

The first reviewable implementation is A1 with its fault/rebuild tests; A2/B follows
with the durable ownership API and real permit-release tests. A green baseline or
this allocation document alone does not satisfy either implementation gate.

## A1 implementation in #469

Baseline: `aa28e92c75818519e8d5b4a49e4002217de1f2c3`. Migration 0118 is reserved
for terminal projection and prune receipts; master and every open PR were
checked again before writing it. Both database registries and schema-version
metadata register 118. Runtime forwarding activation is gated on Actions;
registered replay and its acceptance entry point land first.

The winning request CAS still owns normalization, pricing, reservation settlement,
the financial ledger, prepaid budgets/account lifetime usage, terminal cause,
archive generation fence and settlement feed. Its commit inserts a typed terminal
snapshot with the same request/reservation identity. No lossy queue owns evidence.
Duplicate financial recovery can return the retained result even after request
row/locator retention. A conflicting outbox insert aborts the financial transaction.

Terminal claimers take at most 32 rows, use PostgreSQL `SKIP LOCKED`, and receive
a five-minute lease. Owners are fresh batch nonces; an expired nonce cannot
reclaim its own task and accidentally revive an older attempt. Replay takes the
shared statistics fence before task/fact locks. Facts, aggregates, the committed
event cursor and statistics acknowledgement commit together. A preceding short
transaction applies deferred account usage and both account receipts together,
without acquiring a statistics or event lock. Account updates therefore survive
a failed statistics attempt, and waiting for a nonfinancial lock holds no account
lock against a financial writer. This also avoids reversing legacy account/cursor
lock ordering during rolling upgrades.
`account_projected_at` and `statistics_outcome` distinguish financial projection
from an explicitly pruned statistical scope.

New conversation envelopes contain a bounded fingerprint (at most 1024 hashes)
and a Merkle leaf, plus identity and declared hints. They reference immutable
content/prefix rows prepared before the final financial transaction; they do
not duplicate the request body. Prefix traversal for background classification
uses those durable nodes. Request row/locator retention does not prevent fact
reclassification or projected-event publication. This change does not implement
request body spooling or eliminate synchronous immutable content preparation.

Legacy metered/conversation workers cannot safely consume new tasks: a metered
worker could add the new terminal's aggregates a second time, and a conversation
worker cannot interpret the compact semantic envelope. Their old lease fields
therefore retain a `terminal-v118` marker with a nonexpiring sentinel. New terminal
replay owns the marked metered account receipt; conversation replay uses separate
v118 lease fields. Old selectors cannot claim these tasks, including after a new
worker crashes. A crashed new worker is recovered through the new expiring lease,
never by releasing the legacy marker. Pending metered amounts remain visible to
account usage snapshots throughout, and are never skipped by a statistics prune.

The prune driver holds the exclusive background statistics fence, rejects
unacknowledged terminal/conversation/metered work in the target scope, and commits
a monotonic global `before_day` tombstone with deletion. Financial enqueue does
not lock the tombstone: a concurrent commit can miss the prune snapshot, but
replay must read the committed tombstone under the shared fence. Such a task gets
an explicit `pruned` statistics receipt while its account update still completes.
Events and financial receipts are not observability facts and are not deleted by
this operation. Session totals are recomputed from surviving facts.

Day rebuild refuses pruned days, never synthesizes pending terminal facts from
live sources, and preserves acknowledged terminal facts whose raw sources have
expired. Legacy pending metered facts prevent rebuilding that day until their
consumer finishes. Thus facts and aggregate acknowledgement share one inclusion
boundary; neither a timestamp watermark nor a maximum UUID stands in for receipts.
Maintenance uses READ COMMITTED after acquiring the exclusive fence: an advisory
lock query that waits under SERIALIZABLE can establish its snapshot before an
earlier projector commits. Reading after fence acquisition must see that commit.
The current maintenance scripts must accompany the runtime change; old maintenance
scripts do not implement this protocol.

Payloads, content dependencies and receipts are retained; no outbox GC is added.
Future GC must account for every consumer and the financial recovery boundary.
Actions acceptance includes actual prune/day-rebuild SQL, source deletion,
claim expiry, stale owners, duplicate recovery, account/conversation ordering,
acknowledgement rollback and both PostgreSQL locks. Existing 1024-request tests
retain their sizes. No local build/test/install or production operation is part
of validation. A2 permit transfer, finalization capacity admission and body
spooling are still unimplemented; no forwarding concurrency is reduced.
