-- Deployment dependency: 0079 from PR16 must precede this stacked migration.
-- Immutable audit + replay receipt, intentionally separate from mutable job
-- result_json. No request/provider payloads or raw idempotency keys are stored.
ALTER TABLE generation_jobs ADD COLUMN delivery_confirmed_at BIGINT;
ALTER TABLE generation_jobs ADD COLUMN reconciliation_deadline_at BIGINT;

CREATE TABLE generation_quarantine_resolutions (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    job_id TEXT NOT NULL,
    submission_nonce TEXT NOT NULL,
    actor_service_id TEXT NOT NULL,
    idempotency_hash TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    expected_revision TEXT NOT NULL,
    action TEXT NOT NULL CHECK (action IN ('confirmed_not_submitted', 'confirmed_submitted')),
    evidence_digest TEXT NOT NULL,
    upstream_job_id TEXT,
    result_json TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    UNIQUE(job_id, submission_nonce),
    UNIQUE(tenant_id, actor_service_id, idempotency_hash),
    CHECK ((action = 'confirmed_not_submitted' AND upstream_job_id IS NULL)
        OR (action = 'confirmed_submitted' AND upstream_job_id IS NOT NULL))
);

CREATE INDEX generation_quarantine_pending
    ON generation_jobs (tenant_id, id)
    WHERE status = 'submitting' AND error_code = 'shutdown_delivery_unknown';
