# CPA to Token Center cutover

This runbook keeps CPA available until the new gateway and its imported history
have been validated. It assumes the incremental importer has already completed a
full baseline import.

## Preconditions

- Production PostgreSQL and S3 satisfy the HA and backup gates in the other
  operations documents.
- A recent restore drill succeeded with the exact application image digest.
- Argo CD is synced and healthy, the migration hook succeeded, all expected pods
  are Ready, and there are no database pool acquisition errors.
- Dashboards and alerts cover gateway errors/latency, database pool, archive
  gaps, generation queue and import lag.
- The old CPA Deployment and PVC remain unchanged and recoverable.
- Existing CPA credentials resolve to stable Token Center identities with the
  expected historical request counts and policy.

## Final delta

1. Run one incremental import while CPA remains live and record the source
   watermark and target count.
2. At the change window, stop accepting new CPA traffic or otherwise establish a
   source write barrier. A SQLite WAL that is still changing is not a consistent
   cutover boundary.
3. Take the approved CPA backup/snapshot and run the final incremental import.
4. Re-run it once to prove idempotency: the target request and aggregate counts
   must not increase on the second run.
5. Confirm the import watermark is at or beyond the source's final event and that
   every expected legacy credential maps to one stable key identity.

The importer must have only one active instance. Until the import script uses a
database advisory lock, the runbook and Job controller must prevent concurrent
runs.

## Traffic shift

1. Send synthetic OpenAI, Anthropic, image and asynchronous generation requests
   through a canary credential and verify permission, rate limit, accounting,
   archive and self-service views.
2. Shift a small client cohort to Token Center. Compare upstream response errors,
   latency and accounting with CPA.
3. Increase traffic in bounded steps. Keep CPA deployed but do not dual-submit a
   billable request unless the client has an explicit idempotency contract.
4. After the observation window, switch the remaining clients. Keep the write
   barrier and final import evidence.

## Rollback

Rollback routes clients to CPA; it must not reverse database migrations or delete
Token Center records. Record the Token Center traffic interval so any later
reconciliation can distinguish requests that never reached CPA. Preserve both
data stores and archive buckets read-only as needed for investigation.

Abort and roll back when any of these occurs:

- credential identity, permissions, balances or historical counts disagree;
- sustained error/latency thresholds are exceeded;
- database pool acquisition errors, archive gaps or generation queue age grow;
- the final import cannot establish a fixed watermark;
- PostgreSQL, object storage or proxy loses redundancy.
