ALTER TABLE generation_jobs ADD COLUMN routing_snapshot TEXT;
ALTER TABLE request_records ADD COLUMN routing_snapshot TEXT;
ALTER TABLE request_records ADD COLUMN submission_started_at BIGINT;
ALTER TABLE request_records ADD COLUMN submission_uncertain_at BIGINT;
ALTER TABLE generation_quarantine_resolutions ADD COLUMN actor_credential_generation BIGINT;
CREATE INDEX request_records_image_submission_pending_idx
    ON request_records (tenant_id, id)
    WHERE submission_started_at IS NOT NULL AND completed_at IS NULL;

CREATE TABLE image_generation_quarantine_resolutions (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    request_id TEXT NOT NULL UNIQUE,
    actor_service_id TEXT NOT NULL,
    actor_credential_generation BIGINT NOT NULL CHECK (actor_credential_generation > 0),
    idempotency_hash TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    expected_revision TEXT NOT NULL,
    action TEXT NOT NULL CHECK (action IN ('not_delivered', 'settle_confirmed')),
    confirmed_cost_micros BIGINT NOT NULL CHECK (confirmed_cost_micros >= 0),
    currency TEXT NOT NULL,
    evidence_digest TEXT NOT NULL,
    result_json TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    UNIQUE (tenant_id, actor_service_id, idempotency_hash)
);
