# Gateway persistence isolation: first delivery

Audited base: master `3951689c`. This delivery isolates **request body archive
capture** from gateway dispatch. It does not complete the ledger P0 architecture.

## Implemented boundary

`api/proxy.rs` now calls `start_proxy_request_with_deferred_archive`. The primary
database transaction still atomically reserves usage, inserts the request identity
and locator, and emits the started event. Dispatch requires a positively observed
commit. Failed or ambiguous safety admission never dispatches. Authentication,
rate/credit/concurrency admission, duplicate request identity, submission fencing,
delivery barriers and terminal settlement have not been made best effort.

After that commit, optional request capture uses nonwaiting semaphore admission:

- At most 16 queued plus executing jobs per `Database` instance; clones share it.
- At most 32 MiB of copied request body bytes; individual capture limit 16 MiB.
- One executing writer and one separate lazy database connection. It never
  acquires a connection from the safety pool.
- Two seconds total lifetime, including writer queue time; pool acquisition
  timeout 250 ms. PostgreSQL statement/lock/idle transaction limits are
  250 ms / 100 ms / 1 s; SQLite busy timeout is 100 ms.
- Compression/encryption and archive budget acquisition happen in the task.
  No caller waits for queue space, archive writes, retry, or cleanup.

Full queue, byte budget, unavailable optional database, timeout and process exit
may lose optional request body bytes. The safety transaction already committed
the canonical `gap://<request-id>/request` locator. A successful capture is sealed
atomically in the existing encrypted spool and later uploaded by its existing
fenced worker. A request that settles before capture remains eligible only with
the exact tenant, reservation and canonical gap locator. Already-bound terminal
objects cannot be reopened.

Archive capacity continues using durable private budget reservations and existing
purpose limits. Cancellation does not spawn unbounded cleanup tasks: these jobs
leave refunds to the durable ten-minute reservation expiry/recovery path when
explicit cleanup cannot finish. A timeout during COMMIT may still have committed
the spool; it is not retried or deleted on that assumption. Queue exhaustion never
undoes or settles the safety reservation.

The 32 MiB limit counts retained plaintext; one writer additionally owns bounded
ciphertext batches/nonces/compression buffers. It is not a total process RSS
limit. Degradation logs contain request IDs and bounded reason categories, never
payloads. Existing request gap counters cover immediate queue/retention rejection;
asynchronous errors are currently logs, not new Prometheus counters. Retention
rejection and queue/database failure retain the gap locator, not a body digest.
Durable-capacity rejection still records the existing digest evidence when its
optional transaction succeeds. There is no replay of unpersisted body bytes.

## Audit: remaining synchronous boundaries

| Phase | Current blocking work | Disposition |
| --- | --- | --- |
| Authentication/admission | key lookup, budget reservation, request record/locator and started event/global cursor | Safety record stays synchronous; started event is still coupled and needs a later outbox/projector change |
| Request body preparation | retention JSON parsing/encoding and copying admitted bytes | No database wait; CPU and existing ingress memory admission remain synchronous |
| Dispatch | candidate reassignment, submission/delivery fences and routing/session writes | Unchanged; not all are proved necessary safety state |
| Buffered terminal/first downstream byte | response preseal/spool budget/capture, conversation work, request statistics writer lock, rollups, finished event and settlement | Still coupled in `finish_proxy_request_inner`; can block buffered delivery |
| SSE lifecycle | response spool initialization/appends, delivery confirmation, terminal settlement and EOF archive barrier | Existing response capture isolation is not redesigned; terminal/EOF can still block |
| Diagnostics/audit | upstream probe/health and routing audit persistence | Not exhaustively isolated by this patch |

The optional pool shares the same physical database. PostgreSQL server-wide
failure, shared budget contention and SQLite's single writer can still affect
safety and response persistence; a separate pool is not a separate storage
failure domain. A wholly unavailable primary database must fail closed. There is
no promise to forward unauthenticated or unreserved requests during such failure.
No shutdown drain is promised for optional jobs. Database close terminates the
optional pool; already committed safety facts remain the recovery authority.

The preexisting synchronous archive admission helper remains test-only for its
transaction rollback/cancellation contracts; gateway production uses the deferred
entry point. Tests of that legacy helper do not establish gateway isolation.

## Acceptance and validation

Only GitHub Actions runs tests/builds. Local actions are source review, formatting
and `git diff --check`; no local compilation or automated tests.

- `db::gateway_persistence::tests`: occupied optional pool with a single safety
  connection, closed optional pool, count/byte saturation, queue expiry, duplicate
  admission rollback, failed safety admission, capture after terminal and fencing
  against an already-bound object. Verify durable reservation/gap and permit return.
- `api::proxy::tests::gateway_persistence`: rejected request archive inserts and
  queue saturation preserve exact SSE bytes, one upstream request, and the existing
  exactly-once settlement assertions.
- `postgres_archive_budget_lock_does_not_block_upstream_dispatch`: actual locked
  shared archive budget, upstream receives the request before lock release.
  Does not claim buffered response completion under that shared lock.
- `postgres_request_archive_table_lock_does_not_block_response_body`: exclusive
  request chunk table lock held through complete buffered response delivery.
- Existing `durable_admission_database_failure_never_dispatches_upstream` protects
  fail-closed safety admission; full CI retains legacy archive cancellation,
  budget, terminal and settlement tests.

Not covered by new tests: process kill in every COMMIT phase, deployment-wide
multi-instance memory limits, production latency percentiles, whole-database
network blackholes, response archive/terminal-statistics isolation, every provider
adapter, and process shutdown/drain. PostgreSQL tests require the CI service URL;
they skip without `MTC_TEST_POSTGRES_URL`.
