-- Records whose stable key/principal identity cannot be proven are retained in
-- an operator-only quarantine.  These tables deliberately have no nullable
-- key reference in the record itself and are never joined by self-service,
-- request statistics, billing, or conversation queries.
CREATE TABLE IF NOT EXISTS session_archive_quarantine_batches (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    source TEXT NOT NULL,
    cpamp_source TEXT NOT NULL,
    source_digest TEXT NOT NULL,
    source_size_bytes BIGINT NOT NULL,
    eligible_records BIGINT NOT NULL,
    quarantine_records BIGINT NOT NULL,
    tenant_binding_kind TEXT NOT NULL,
    tenant_binding_proof TEXT NOT NULL,
    approved_by_service_id TEXT,
    created_at BIGINT NOT NULL,
    UNIQUE (tenant_id, source, source_digest)
);

CREATE TABLE IF NOT EXISTS session_archive_quarantine_records (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    source TEXT NOT NULL,
    cpamp_source TEXT NOT NULL,
    external_request_id TEXT NOT NULL,
    record_digest TEXT NOT NULL,
    identity_claim_digest TEXT,
    reason_code TEXT NOT NULL CHECK (
        reason_code IN ('missing_credential_hash', 'unproven_identity')
    ),
    proof_digest TEXT NOT NULL,
    source_started_at BIGINT NOT NULL,
    source_completed_at BIGINT,
    protocol TEXT NOT NULL,
    model TEXT NOT NULL,
    status_code BIGINT,
    duration_ms BIGINT,
    input_tokens BIGINT NOT NULL DEFAULT 0,
    output_tokens BIGINT NOT NULL DEFAULT 0,
    error_code TEXT,
    request_digest TEXT,
    response_digest TEXT,
    request_object TEXT,
    response_object TEXT,
    quarantined_at BIGINT NOT NULL,
    UNIQUE (tenant_id, source, external_request_id),
    UNIQUE (tenant_id, source, proof_digest)
);

CREATE INDEX IF NOT EXISTS session_archive_quarantine_page_idx
    ON session_archive_quarantine_records
       (tenant_id, source_started_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS session_archive_quarantine_source_idx
    ON session_archive_quarantine_records
       (tenant_id, source, source_started_at, external_request_id);
-- A record can appear in more than one sealed overlap input.  Keep canonical
-- record identity separate from immutable batch membership instead of changing
-- the first-seen batch on replay.
CREATE TABLE IF NOT EXISTS session_archive_quarantine_batch_records (
    tenant_id TEXT NOT NULL,
    batch_id TEXT NOT NULL,
    quarantine_id TEXT NOT NULL,
    sequence BIGINT NOT NULL,
    created_at BIGINT NOT NULL,
    PRIMARY KEY (batch_id, quarantine_id),
    UNIQUE (batch_id, sequence)
);

CREATE INDEX IF NOT EXISTS session_archive_quarantine_batch_record_idx
    ON session_archive_quarantine_batch_records (quarantine_id, batch_id);

-- A resolution is append-only and final.  The application exposes INSERT only;
-- the unique quarantine_id makes a later contradictory decision impossible.
-- Association does not mutate the quarantine row.  A subsequent sealed import
-- may project the record to the proven stable key while retaining this audit.
CREATE TABLE IF NOT EXISTS session_archive_quarantine_resolutions (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    quarantine_id TEXT NOT NULL UNIQUE,
    action TEXT NOT NULL CHECK (action IN ('associate', 'dismiss')),
    key_id TEXT,
    evidence_digest TEXT NOT NULL,
    note TEXT,
    idempotency_key TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    resolved_by_service_id TEXT,
    created_at BIGINT NOT NULL,
    UNIQUE (quarantine_id, idempotency_key),
    CHECK (
        (action = 'associate' AND key_id IS NOT NULL)
        OR (action = 'dismiss' AND key_id IS NULL)
    )
);

CREATE INDEX IF NOT EXISTS session_archive_quarantine_resolution_tenant_idx
    ON session_archive_quarantine_resolutions (tenant_id, created_at DESC, id DESC);
