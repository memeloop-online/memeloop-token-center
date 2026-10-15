# Conversation prefix storage root fix — 2026-10-15

## Scope and evidence

Base: `origin/master` ee285c49, includes #626/schema126 and #629. Isolated branch
`fix/conversation-prefix-storage-20261015`. No changes to UI, billing, pool #628,
production, volumes, PostgreSQL settings, backups, historical rows or Wiki.
No local tests, build, check or dependency installation. Independent reviews
are reserved for the parent; no subagents were started.

Audit source: `/var/lib/mwc/build-scratch/tmp/mtc-20261008/storage-capacity-prefix-audit-20261015.{md,json}`.
Business database 24,966,556,819 bytes; observations 8,459,657,216 bytes including
7,653,449,728 bytes TOAST. This does not establish that all TOAST is redundant or
reclaimable. Volume is already approximately 80 GiB. No claim of immediate 7 GB recovery.

## Existing semantics

`materialize_conversation_content_with_statement_error` batches tenant-scoped
semantic atoms and content-addressed prefix nodes. Observations retain a full
Merkle leaf but also repeat a serialized inference fingerprint. Current master
already caps new fingerprints to 1024 atoms/70,000 bytes; old rows may be larger.
The 50-row fallback and five indexed structured candidate lookups are the only
product readers of this JSON. History/detail, session summaries and billing
read observation metadata rather than this JSON. Explicit parent/session/turn,
branch, compaction overlap and key isolation must remain intact.

Deferred projection persists the old semantic snapshot and optional durable
payload, materializes content in a separate transaction, then atomically writes
the observation, request membership and acknowledgement. The representation
change does not alter the durable outbox envelope or its replay comparison.
Archive-only inputs use the same transactional observation path with
`attach_request_record=false`; no CPA migration tooling is added here. External
importers that directly read this field must be inventoried before activation.

## Implementation design

Keep the existing full `leaf_node_hash`. Replace the repeated inference array,
when activated, with `{ "context_prefix_v1": <existing node hash or null>,
"atom_count": <exact bounded fingerprint length> }`. The referenced node is the
end of the existing first 1024 atoms, not necessarily the full leaf. No second
prefix tree, shared-body copy, new content store or empty-list sentinel.

Readers accept legacy arrays and explicit references. Oversized historical
arrays are represented as SQL NULL internally and rebuilt through their full
leaf, rather than silently treated as empty. One tenant-scoped recursive query
loads all required heads plus the current projected leaf. `UNION` shares common
ancestors, and primary-key lookups follow only those paths. No SQL inside the
candidate loop. Each loaded path validates depth, root termination and Merkle
hash identity; missing, malformed or inconsistent paths fail closed.

Before publishing a compact observation, the complete current leaf path must
exist and its inference prefix must equal the snapshot/input. Thus a caller's
`content_materialized=true` assertion alone cannot publish a dangling reference.
Existing content materialization order and transaction/lock ordering remain.

## Rolling compatibility and activation (not executed)

Migration 127 creates a single policy row with `reference_writes=0`. It neither
rewrites historical rows nor activates compact writes. Deploy reader support
first, retaining legacy array writes. Inventory and upgrade every reader,
gateway, worker, importer and replay process; drain all older processes and
verify their absence. Only a separately authorized change may set the policy
to 1. This is an operational two-phase fence, not automatic old-process detection.
Enabling it while old readers exist is unsupported: old code silently interprets
non-array JSON as empty. The present PR must not be presented as authorization
to activate it, or as a rolling-safe single-step enablement.

After activation, setting the policy back to 0 stops new compact writes but
does not make existing compact rows readable by old binaries. Keep compatible
readers, or separately plan and validate lossless expansion before an old-reader
rollback. Old writers may still emit valid arrays; new readers accept both.

## Validation and status

First reviewable design/source progress recorded at approximately 13:57 UTC
(21:57 Beijing), two minutes after the requested first-progress target.
At that point implementation was incomplete and no checks had run. Dedicated
SQLite/PostgreSQL long-prefix, reconstruction, isolation and replay evidence
and the existing GHA checks are required before calling this complete.

## Separate future reclamation plan (not authorized here)

First measure array size, reference coverage and full-path integrity with a
separately approved, bounded tenant-aware audit. Estimate actual eligible bytes,
including index/heap effects and TOAST compression; do not equate the whole
TOAST relation to reclaimable bytes. A later dedicated change could backfill
eligible arrays in bounded resumable batches, preserving leaf/content/request
and financial data, with explicit rollback and replay checks. Space-return
procedures require their own capacity, lock and operational authorization.
No backfill, history deletion or space-return command is included or executed.
