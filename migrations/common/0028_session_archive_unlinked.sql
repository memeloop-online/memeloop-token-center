CREATE TABLE IF NOT EXISTS session_archive_correlations (
    tenant_id TEXT NOT NULL,
    source TEXT NOT NULL,
    external_request_id TEXT NOT NULL,
    disposition TEXT NOT NULL CHECK (disposition IN ('exact', 'unlinked')),
    key_id TEXT NOT NULL,
    principal_id TEXT NOT NULL,
    target_request_id TEXT,
    target_request_created_at BIGINT,
    external_event_hash TEXT,
    record_digest TEXT NOT NULL,
    proof_digest TEXT NOT NULL,
    identity_proof_kind TEXT NOT NULL,
    identity_proof_digest TEXT NOT NULL,
    source_model TEXT NOT NULL,
    source_started_at BIGINT NOT NULL,
    correlated_at BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, source, external_request_id),
    UNIQUE (tenant_id, source, proof_digest),
    CHECK (
        (disposition = 'exact'
            AND target_request_id IS NOT NULL
            AND target_request_created_at IS NOT NULL
            AND external_event_hash IS NOT NULL)
        OR
        (disposition = 'unlinked'
            AND target_request_id IS NULL
            AND target_request_created_at IS NULL
            AND external_event_hash IS NULL)
    )
);

CREATE INDEX IF NOT EXISTS session_archive_correlations_identity_idx
    ON session_archive_correlations (tenant_id, key_id, principal_id, source_started_at DESC);
CREATE INDEX IF NOT EXISTS session_archive_correlations_target_idx
    ON session_archive_correlations (tenant_id, target_request_id)
    WHERE disposition = 'exact';

-- Archive-only rows intentionally live outside request_records, request locators,
-- request statistics facts and billing aggregates.  They are a conversation and
-- diagnostic projection of a source record, never a second billable request.
CREATE TABLE IF NOT EXISTS session_archive_unlinked_requests (
    tenant_id TEXT NOT NULL,
    source TEXT NOT NULL,
    external_request_id TEXT NOT NULL,
    archive_request_id TEXT NOT NULL,
    key_id TEXT NOT NULL,
    principal_id TEXT NOT NULL,
    conversation_cluster_id TEXT,
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
    imported_at BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, source, external_request_id),
    UNIQUE (archive_request_id)
);

CREATE INDEX IF NOT EXISTS session_archive_unlinked_key_cluster_page_idx
    ON session_archive_unlinked_requests
       (key_id, conversation_cluster_id, source_started_at DESC, archive_request_id DESC);
CREATE INDEX IF NOT EXISTS session_archive_unlinked_tenant_request_idx
    ON session_archive_unlinked_requests (tenant_id, archive_request_id);
CREATE INDEX IF NOT EXISTS session_archive_unlinked_source_watermark_idx
    ON session_archive_unlinked_requests
       (tenant_id, source, source_started_at, external_request_id);
