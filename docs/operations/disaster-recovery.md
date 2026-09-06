# Disaster recovery

## Scope

Token Center production state consists of PostgreSQL records, the S3-compatible
archive bucket, the release's immutable image digest, Helm values, and the
references to runtime Secrets. A recoverable backup records all of these
identifiers without copying credentials into Git or incident tickets.

## Backup requirements

- Take PostgreSQL backups with point-in-time recovery enabled and record the
  backup identifier, restore point, database version, and schema version.
- Version the archive bucket and verify that the backup policy covers request
  bodies and generated assets. Record an object inventory checksum and the
  bucket version or snapshot identifier.
- Retain the active service and plugin-installer digests, the Helm values
  revision, and the Secret-reference names needed to recreate the runtime.
- Test restoration in an isolated namespace and account. Never point a restore
  rehearsal at the live database or archive bucket.

## Recovery

1. Stop writers and preserve the incident's immutable logs and release digest.
2. Restore PostgreSQL to the chosen recovery point in an isolated target, then
   restore or verify the matching archive object versions.
3. Run the release's schema compatibility checks before admitting traffic.
4. Verify stable credential lookup, request history, usage aggregates, archive
   access, readiness, and a bounded authenticated canary.
5. Switch traffic only after the new target is healthy. Retain the previous
   target and its routing configuration until the recovery receipt is complete.

Schema rollback is restore-based. Do not reverse applied DDL in place after
writers have committed data. Credentials and plaintext archive bodies must not
appear in recovery commands, Git history, or reports.
