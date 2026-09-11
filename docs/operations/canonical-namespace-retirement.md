# Canonical namespace data-plane migration and trial retirement

The reversible API2/trial namespace is not a canonical data plane.  In
particular, a canonical runtime must not keep PostgreSQL, MinIO/S3, or its
operator ingress in that namespace after promotion.  This procedure is a
planning and evidence gate; it contains no command that deletes CPA, CNPG,
PVCs, buckets, or a namespace.

## Blue/green data-plane sequence

1. Provision an independent canonical CNPG cluster/PVC set and an independent
   canonical object-store bucket/prefix.  Do not point the canonical workload
   at trial DNS names, trial PVCs, or trial bucket credentials.  Record only
   non-secret endpoint identities and the target resource UIDs in the private
   change record.
2. Restore PostgreSQL to the green cluster from a verified base backup, replay
   WAL through an explicit recorded checkpoint, and run read-only schema and
   application reconciliation.  A copied SQLite main file, an unpaired WAL,
   or a CPAMP watermark alone is not a source checkpoint.
3. Replicate immutable archive objects into the green bucket, record a sorted
   count/byte/digest inventory, then repeat incremental object syncs while the
   trial remains live.  The final object sync is performed only after the
   approved write barrier and drain; its inventory must match the green
   database locators.  Keep both source stores intact for rollback.
4. Run CPAMP and session-archive dry-run, apply, and exact replay against the
   canonical target.  Reconcile CPAMP links, archive checkpoint/correlations,
   quarantine, and `gap://` locators.  A zero archive checkpoint, an unresolved
   gap, or a lagging WAL checkpoint blocks promotion.
5. Reconcile stable client identities by count, not plaintext.  The receipt
   requires `client_keys_expected == client_keys_attached` and
   `recovery_envelopes_expected == recovery_envelopes_verified`; therefore an
   inventory such as 11 client keys but 10 recovery envelopes fails closed.
6. Validate a restore of the green PostgreSQL/object-store pair.  Keep the
   trial pair read-only and available as rollback evidence before changing any
   host.
7. Move the private `token-operator` host to the canonical control service.
   Verify the canonical host serves the intended private ingress and the trial
   host is detached.  Public traffic is a separate approved cutover and is not
   implied by this operator-host change.
8. At the final write barrier, drain writers, record the final PostgreSQL WAL
   and object-sync checkpoints, repeat reconciliation, and only then promote
   canonical traffic.  No source data resource is removed in this step.

## Read-only gate

Create an owner-private, non-secret JSON receipt with this exact shape (the
digest fields are lower-case SHA-256 values; counts may be JSON numbers or
decimal strings):

```json
{
  "schema_version": 1,
  "canonical_namespace": "token-center",
  "trial_namespace": "token-center-api2-trial",
  "operator_host": "token-operator.example.invalid",
  "final_write_barrier": true,
  "postgres": { "restored": true, "catchup_verified": true, "source_wal_checkpoint_digest": "...", "canonical_wal_checkpoint_digest": "..." },
  "object_store": { "restored": true, "final_sync_verified": true, "source_inventory_digest": "...", "canonical_inventory_digest": "..." },
  "migration": { "cpamp_reconciled": true, "archive_reconciled": true, "no_gap_locators": true, "archive_checkpoint": 1, "client_keys_expected": 11, "client_keys_attached": 11, "recovery_envelopes_expected": 11, "recovery_envelopes_verified": 11 },
  "operator_cutover": { "canonical_host_serves": true, "trial_host_detached": true },
  "rollback": { "canonical_restore_tested": true, "trial_data_retained": true },
  "prune": { "approved": false, "retention_elapsed": false, "trial_data_export_verified": false }
}
```

Run the following using a least-privilege Kubernetes identity that can list
only the named resources.  It reads no Secret object and has no write/delete
code path.  Its output contains the exact non-secret resource
`apiVersion`/kind/name/UID inventory plus a digest; it never emits Secret data,
environment values, database URLs, or credentials.

```sh
node ops/release/audit-canonical-retirement.ts \
  --canonical-namespace token-center \
  --trial-namespace token-center-api2-trial \
  --operator-host token-operator.example.invalid \
  --receipt /private-evidence/canonical-retirement-receipt.json
```

The tool deliberately reports `can_prune: false` while any trial CNPG, pool,
or PVC remains.  It is not a deletion tool.  A later, separately approved
prune change must re-list and match the retained exact UID inventory, prove the
retention/export period has elapsed, and explicitly exclude CNPG, data PVCs,
and object-store buckets until their own backup-retention policy authorizes
removal.  Never delete those resources as a shortcut to namespace cleanup.
