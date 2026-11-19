-- Supplier evidence is not an independent probe result. Empty model means
-- account scope, and may only be written with explicit provider-wide evidence.
CREATE TABLE upstream_supplier_availability (
    upstream_account_id TEXT NOT NULL REFERENCES upstream_accounts(id) ON DELETE CASCADE,
    upstream_model TEXT NOT NULL,
    credential_generation BIGINT NOT NULL,
    transport_revision BIGINT NOT NULL,
    failure_kind TEXT NOT NULL,
    reset_at BIGINT,
    reset_source TEXT NOT NULL,
    observed_at BIGINT NOT NULL,
    request_id TEXT NOT NULL,
    -- Retained evidence has its own age; a later weak 429 is not a renewal.
    hard_observed_at BIGINT,
    reset_observed_at BIGINT,
    success_at BIGINT,
    verification_after BIGINT,
    PRIMARY KEY (upstream_account_id, upstream_model, credential_generation, transport_revision)
);

-- Do not backfill independent provenance: old status/timestamps could come
-- from customer traffic. Readback projects these untouched columns as
-- legacy_unverified historical_result until a real probe supplies result_json.
ALTER TABLE upstream_service_probes ADD COLUMN result_json TEXT;
ALTER TABLE upstream_service_probes ADD COLUMN last_attempt_json TEXT;
ALTER TABLE upstream_service_probes ADD COLUMN last_attempt_at BIGINT;
ALTER TABLE upstream_service_probes ADD COLUMN actual_model TEXT;
ALTER TABLE upstream_service_probes ADD COLUMN credential_generation BIGINT;
ALTER TABLE upstream_service_probes ADD COLUMN transport_revision BIGINT;
