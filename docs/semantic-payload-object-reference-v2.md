# Semantic payload object references

Version 1 remains the original inline document. Version 2 stores an existing
`ConversationBodyReference` whose immutable tenant CAS locator explicitly names
the configured training backend. `encoded_bytes`, `digest`, tenant, request, key
and principal retain their original plaintext-document meaning. They never become
metadata about the reference JSON. No outbox request/replay digest or financial
responsibility changes in this reader patch.

Migration 153 widens only the format CHECK to permit 1 and 2. PostgreSQL uses a
NOT VALID widened constraint because all historical rows were already restricted
to 1; it enforces subsequent writes without a full scan of TOAST bodies. SQLite
uses its existing transactional table replacement pattern and copies every row
and original relationship unchanged. Existing payload-length constraints remain.

The background worker dispatches by version before parsing body content. It
releases the source query connection, opens training CAS, streams length and
BLAKE3 verification, restores UTF-8, checks character count and the original
semantic digest, and only then materializes existing conversation content. It
retains a small source token containing the exact stored-reference hash, version,
original digest/length and identities. Under the existing final projection lease
and row lock it rechecks that token before recording/acknowledging work or deleting
the payload. This applies to reduced-sentinel recovery as well as declared payload
snapshots. No object-store read occurs in that transaction. A changed source,
missing object/backend, wrong identity or failed stream leaves the outbox pending.

Reader-compatible binaries and the 841 training configuration must reach every
worker/reader before an operator writes any version 2 row. Merely applying schema
153 does not convert a row, change the global S3 endpoint or enable new CAS writes.
Historical migration is a separate tools command: a committed immutable copy
receipt and verified target bytes must exist before a short source-row lock can
compare current original source version/hash/length and replace its inline value
with the qualified ref. Concurrent changes are skipped, source archives and all
receipts are retained, and unknown commits require re-reading the actual row.
No full-service write barrier, pending deletion, VACUUM FULL, staging GC change,
archive request gate, new encryption or manufactured plaintext is introduced.
