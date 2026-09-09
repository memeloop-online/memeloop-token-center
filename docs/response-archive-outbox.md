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
- The response locator and existing archive staging binding are committed
  together under tenant/request/reservation and lease fencing. No upstream
  request is replayed and billing fields are not changed.
- Lost seal ACKs cannot turn a committed pending spool into a gap. Lost
  bind ACKs cannot undo a committed archive binding.
- Successful binding allows exact chunk cleanup; unbound chunks are only
  cleaned after expiry. Audit metadata remains, like request history, and
  requires its own operational retention/storage planning.

The existing API may report `archive_complete=false` while a sealed spool
is pending upload. This patch does not introduce new Requests UI fields.
Permanent gap, pending and bound are distinct internal spool states.

## Release gates

No local build or tests were run. Added tests cover encryption ownership,
same-batch nine-frame ingestion without an object-store consumer, CRLF
capture/replay, incomplete-prefix rejection, budget limits, stale leases,
lost ACKs and exact cleanup. CI must execute them, including migration and
PostgreSQL coverage, before deployment.

The global accounting transaction intentionally serializes admission and
chunk mutations; realistic database latency/concurrency and S3 outage
runtime checks remain mandatory. Filling the budget must cause explicit
gaps, not unbounded memory or billing retries.

Before integration, update the product Helm `migration.schemaVersion` in
`values.yaml` and the corresponding `values.schema.json` constant to 71
together. GitOps vendored/default/runtime schema metadata must likewise
be aligned during the separately authorized release. This branch does
not run migrations, alter deployment configuration or modify secrets.
