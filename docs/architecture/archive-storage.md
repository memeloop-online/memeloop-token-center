# Archive storage and SlateDB decision

## Decision

Token Center keeps three separate responsibilities:

1. PostgreSQL (SQLite in tests) stores authorization boundaries, request metadata,
   usage facts and rollups, archive references, and logical-conversation
   projections.
2. `ArchiveStore`, backed by the Apache Arrow `object_store` crate, stores the
   immutable request, response, image, and video bodies in S3/MinIO. Filesystem
   and memory backends remain test-only alternatives.
3. Merkle-prefix nodes are evidence used to relate requests into a logical
   conversation. They are not a key-value database and never replace the
   relational tenant/key authorization checks.

SlateDB is therefore **not** a direct replacement for the current archive or
conversation implementation. The gateway, control plane, and generation workers
must not independently open the same SlateDB namespace.

## Why

[SlateDB](https://github.com/slatedb/slatedb) is an object-storage-backed LSM
key-value store and can use MinIO. That makes it a plausible compact directory
or cold index, but it does not provide the joins, dimensional aggregation, tenant
constraints, and transactional quota/billing updates required by Token Center.
Replacing PostgreSQL with it would move those responsibilities into application
code. Wrapping the existing immutable archive bodies in it would also add WAL,
manifest, compaction, and writer-ownership failure modes without removing the
object store.

The current split also keeps large bodies out of PostgreSQL while preserving
bounded, index-backed list and statistics queries. Unlinked historical bodies
remain explicit archive-only records and cannot accidentally enter billing or
usage rollups.

## Durable staging cleanup

Completed multipart objects first live under a typed attempt segment:
`staging/<owner-kind>/<owner-uuid>/<purpose>/<attempt-uuid>`. PostgreSQL v35
owns the attempt state, writer and cleanup fencing tokens, retry time, and the
two empty observations. It never supplies an object path to the deletion
boundary. The object-store adapter reconstructs the prefix only from an
`ArchiveStagingKey`, filters lexical S3 listings at a path-segment boundary, and
verifies the same typed segment is empty after deletion. UUID neighbours and
objects outside that segment cannot enter its delete stream.

The archive reaper is an independent Tokio task. Each bounded pass promotes
stale writers, claims a small batch (`FOR UPDATE SKIP LOCKED` on PostgreSQL),
commits an exact reference proof, and only then performs object-store I/O. No
network call holds a SQL transaction. A 15-second object operation deadline is
shorter than the cleanup heartbeat interval and lease; the database also rejects
any finalization after lease expiry. An empty delete result is retained as a
first durable observation and can become `cleaned` only after the stability
window and a new claim plus reference proof.

Object-store failures release the cleanup lease with bounded durable backoff and
one of a fixed set of low-cardinality codes. Logs do not include endpoints,
credentials, lease-owner UUIDs, or object paths. PostgreSQL replicas coordinate
through `SKIP LOCKED`; SQLite cleanup is supported only for a single process.

Incomplete multipart uploads are outside the completed-object listing used by
the reaper. Production S3/MinIO therefore requires an
`AbortIncompleteMultipartUpload` lifecycle rule (one day is recommended).
Ordinary object expiry must never target `staging/`, because bound locators can
remain there for their entire retention life.

## Optional future experiment

SlateDB may be evaluated as an optional archive directory or cold secondary
index only. It is not part of the durable source of truth until all of these
gates pass:

- one fenced writer per namespace with a documented failover protocol;
- crash recovery and manifest-conflict tests on the production MinIO topology;
- concurrent-reader correctness during compaction and writer failover;
- replay of the complete historical archive with bounded memory;
- p95/p99 lookup and list latency, object count, request rate, and storage-cost
  measurements compared with the current PostgreSQL-reference/object-store-body
  design;
- dual-write verification, checksum reconciliation, backup/restore, and a tested
  rollback path;
- a stable upstream SlateDB release policy compatible with the project's upgrade
  and Kubernetes rollout policy.

Until those results demonstrate a material benefit, PostgreSQL remains the
authoritative metadata and analytics store and MinIO remains the archive-body
store.
