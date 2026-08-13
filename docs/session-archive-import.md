# Importing cpa-session-archive

The importer accepts the normalized `archive` JSONL emitted by
`cpa-session-archive` v0.7.x (`schema_version: 2`). Do not copy an active
`archive.sqlite` file without its WAL. Obtain the JSONL through an authenticated
export ticket, or export from a consistent SQLite backup.

## Identity prerequisite

Run the CPAMP import first. For the initial migration, set `CPAMP_OVERLAP_MS` to
cover the complete CPAMP history so `import_request_links` contains every source
`request_id`, event hash, timestamp, model and key hash. Later runs can return to
the one-day default overlap. This full overlap is idempotent and does not duplicate
request or aggregate rows.

Historical archive records need a full 64-character credential SHA-256 in either
`credential_hash` or legacy `key_id`. Apply cpa-session-archive's identity mapping
backfill before exporting records that only have a human label. The importer does
not guess caller identity.

## Safe execution

`ops/import-cpa-session-archive.sh` is a dry run unless
`SESSION_ARCHIVE_APPLY=true`. The dry run scans the complete overlap batch and
stops if a source request does not map uniquely by request id, timestamp, model
and key hash. `SESSION_ARCHIVE_ALLOW_UNMAPPED=true` is an explicit data-loss
override and should not be used for cutover validation.

Required runtime access is:

- a read-only local JSONL export file;
- the target PostgreSQL connection;
- write/list access to the target archive S3 bucket;
- the service's normal configuration secrets, supplied by Secret references and
  never placed in command arguments.

The apply pass writes request/response bodies to the target BLAKE3 CAS before a
transaction replaces `gap://` references. Content-addressed writes are safe to
repeat. A non-gap target object is never overwritten. Checkpoints are isolated by
tenant and source, retain a bounded overlap for late arrivals, and exact replays do
not increment imported counts or duplicate conversation observations.

## Source-format limits

The upstream schema-2 archive export is normalized and rehydrates request and
response bodies, but it does not export `upstream_request`, `parent_response_id`,
`response_id`, or `source_format`. Durable `session_id`, turn/client/request-kind
facets and request Merkle prefixes are used when available. An explicit session
groups observations into one cluster; it does not by itself create a directed
continuation edge. Missing strong ancestry metadata is left as an unconnected
observation rather than fabricated.

The old collector's whole-database export endpoint has no `since` parameter.
Target-side overlap/checkpoint processing is incremental, but acquiring the JSONL
from that endpoint still scans the old archive. A consistent snapshot plus a
source-side time-bounded exporter is needed if the final archive is too large for
one streamed export.

## Verification boundary

The implementation snapshot was exercised with a file-backed SQLite target and
the test CAS: `cargo test --test session_archive_import` passed 1/1 and the two
module unit tests passed 2/2. That integration proves whole-batch fail-closed
preflight, exact CPAMP linkage, dry-run/apply, BLAKE3 body rehydration,
checkpoint overlap, idempotent replay, conversation observation creation and
refusal to overwrite a real archive object.

This is **not** PostgreSQL or live migration evidence. No production/dev
cpa-session-archive JSONL was imported, and no target PostgreSQL/S3 counts have
been reconciled. A release gate must run dry-run, apply, replay and source/target
body/count sampling against an isolated PostgreSQL schema and test bucket before
any live import. The separately passing PostgreSQL CPAMP fixture validates usage
metadata/checkpoints only; it does not validate this body/session importer.
