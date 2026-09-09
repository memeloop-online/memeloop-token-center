# Durable streaming response archive outbox (schema 71)

This change removes S3 upload latency from the streaming producer. Before
delivering a framed batch, the producer waits at most 250 ms for encrypted
chunks to be acknowledged by the database. Encryption reuses the existing
`v2` private-JSON envelope and `key_pepper`; authenticated context binds the
tenant, request, reservation and sequence. There is no new plaintext disk
spool or new key.

A failed admission/append, timeout or incomplete stream leaves an explicit
archive gap without intentionally terminating usable text delivery. This
is bounded waiting, **not** a zero-latency or no-loss guarantee. Database
availability and commit latency still matter. Already lost historical
streams cannot be reconstructed by this change.

Successful SSE terminal frames and HTTP EOF are released only after the
complete capture's seal acknowledgement, or after a bounded attempt to persist
an explicit gap when capture fails. Ordinary text remains incremental. The
terminal tail is bounded to 2 MiB and preserves split CRLF framing; it uses the
same delivery-start accounting as ordinary output. Object upload is still
asynchronous. A process lost inside seal cannot have delivered a successful
terminal, while a seal committed despite a lost acknowledgement remains
recoverable by the worker after the existing request finalizer converges.

## Bounds and recovery

- 64 KiB plaintext chunks; 64 MiB/request; 65,536 chunks/request.
- 256 MiB shared active accounting budget, including ciphertext plus fixed
  512-byte/chunk and 1,024-byte/request overhead estimates. This is not a
  promise that physical database/WAL/index space is capped at 256 MiB.
- Capturing rows expire after 30 minutes without an acknowledged append.
  Sealed encrypted data has a seven-day retention deadline.
- The worker uploads only sealed streams whose exact request identity is
  terminal and still references the expected response gap.
- Upload attempts are leased and renewed; each attempt is bounded to
  120 seconds, with at most ten attempts. Each tick drains at most 32 tasks,
  using the existing shared archive permit budget.
- Each upload query fetches at most 256 chunks and 1 MiB of ciphertext,
  checks the lease in the same statement snapshot, and does not take the
  producer's global budget lock.
- The response locator and existing archive staging binding are committed
  together under tenant/request/reservation and lease fencing. No upstream
  request is replayed and billing fields are not changed.
- Lost seal ACKs cannot turn a committed pending spool into a gap. Lost
  bind ACKs cannot undo a committed archive binding.
- Successful binding allows exact chunk cleanup; unbound chunks are only
  cleaned after retention expiry or an expired tenth-attempt lease.
  Audit metadata remains, like request history, and
  requires its own operational retention/storage planning.
- Cleanup commits at most 64 chunks and 1 MiB of accounting units per
  transaction. A pass performs at most 32 such transactions; interruption
  preserves earlier committed progress. PostgreSQL GC locks only the
  selected spool until its final budget decrement, using `NOWAIT` for the
  reverse-order budget lock; contention rolls back that one small batch.
  Active-only partial indexes exclude retained cleaned audit rows.

The existing API may report `archive_complete=false` while a sealed spool
is pending upload. This patch does not introduce new Requests UI fields.
Permanent gap, pending and bound are distinct internal spool states.

## Release gates

No local build or tests were run. Added tests cover encryption ownership,
same-batch nine-frame ingestion without an object-store consumer, CRLF
capture/replay, incomplete-prefix rejection, budget limits, stale leases,
lost ACKs and exact cleanup. CI must execute them, including migration and
PostgreSQL coverage, before deployment.

The PostgreSQL regression fixtures block the server inside a real deferred
`COMMIT` trigger, observe that state through `pg_stat_activity`, cancel the
client future and then release the server to verify durable recovery.
Additional cases exercise bounded GC rollback/restart, batch reads while
the budget is locked, high-count tiny chunks and active-only query plans.
These are test definitions, not claims that these checks have passed.

The global accounting transaction intentionally serializes admission and
chunk mutations; realistic database latency/concurrency and S3 outage
runtime checks remain mandatory. Filling the budget must cause explicit
gaps, not unbounded memory or billing retries.

GC `NOWAIT` avoids deadlocks and long ownership of the producer budget,
but has no absolute fairness guarantee under sustained saturation.
Deferred-cleanup warnings and eventual capacity release must be included
in the concurrency acceptance gate.

Before integration, update the product Helm `migration.schemaVersion` in
`values.yaml` and the corresponding `values.schema.json` constant to 71
together. GitOps vendored/default/runtime schema metadata must likewise
be aligned during the separately authorized release. This branch does
not run migrations, alter deployment configuration or modify secrets.
