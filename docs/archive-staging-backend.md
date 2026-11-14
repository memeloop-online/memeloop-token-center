# Durable staging backend identity

Migration 155 adds nullable `archive_staging_attempts.backend_identity`.
NULL always means the original legacy store. A non-NULL 32-byte identity (lower
hex in SQL) is exactly the existing training endpoint/bucket/region identity,
not an alias which can be reinterpreted after moving storage.

`MTC_TRAINING_STAGING_WRITES` defaults to false. Apply the migration and deploy
compatible readers, writers and reapers everywhere before enabling it. The
training endpoint and credentials must refer to the independent haixia store;
old chenhouse identities and data remain untouched. Enabling this flag does not
change existing attempts, legacy roots, default CAS writes, or financial ACKs.
Exact retries use the backend already persisted with the attempt, even if new
write defaults subsequently change. A missing or changed training identity
fails without substituting a same-key legacy object.

A qualified staging locator is
`stores/staging/<identity>/staging/<typed-owner>/<owner-uuid>/<purpose>/<attempt-uuid>/<object>`.
The backend is fixed when multipart starts. Finishing retains the qualified
locator. Text CAS promotion remains in that same backend and returns the
existing qualified tenant CAS namespace. Generic `finish` cannot return a bare
CAS key from a training writer. Typed cleanup selects only the backend from the
persisted key; generic delete/prefix-delete cannot remove qualified staging.
No double-store deletion is used to guess ownership.

This change provides staging ownership compatibility, not a legacy object GC
permit. Legacy raw roots, pending/source/financial release proofs, final root
lookup migration and reference-aware deletion remain separate work. Keep new
writes disabled until the actual rollout completes; schema or code merge alone
is not permission to publish new backend-qualified staging rows.
