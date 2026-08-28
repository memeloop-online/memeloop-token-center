ALTER TABLE session_archive_import_records
    ADD COLUMN source_session_id TEXT NOT NULL DEFAULT '';
ALTER TABLE session_archive_unlinked_requests
    ADD COLUMN source_session_id TEXT NOT NULL DEFAULT '';
ALTER TABLE session_archive_quarantine_records
    ADD COLUMN source_session_id TEXT NOT NULL DEFAULT '';

CREATE INDEX session_archive_import_records_source_session_idx
    ON session_archive_import_records (tenant_id, source, source_session_id);
CREATE INDEX session_archive_unlinked_source_session_idx
    ON session_archive_unlinked_requests (tenant_id, source, source_session_id);
CREATE INDEX session_archive_quarantine_source_session_idx
    ON session_archive_quarantine_records (tenant_id, source, source_session_id);

CREATE TABLE session_archive_snapshot_checkpoints (
    tenant_id TEXT NOT NULL,
    source TEXT NOT NULL,
    source_fingerprint TEXT NOT NULL,
    sequence BIGINT NOT NULL CHECK (sequence > 0),
    offline_full_snapshot BIGINT NOT NULL CHECK (offline_full_snapshot IN (0, 1)),
    output_sha256 TEXT NOT NULL,
    prior_output_sha256 TEXT,
    prior_source_ingest_fence BIGINT,
    snapshot_schema_version BIGINT NOT NULL CHECK (snapshot_schema_version IN (1, 2)),
    ingest_fence BIGINT NOT NULL CHECK (ingest_fence >= 0),
    tombstone_safe_after_ingest_fence BIGINT,
    session_set_sha256 TEXT NOT NULL,
    session_count BIGINT NOT NULL CHECK (session_count >= 0),
    request_count BIGINT NOT NULL CHECK (request_count >= 0),
    deleted_session_count BIGINT NOT NULL CHECK (deleted_session_count >= 0),
    applied_tombstones BIGINT NOT NULL CHECK (applied_tombstones >= 0),
    deleted_records BIGINT NOT NULL CHECK (deleted_records >= 0),
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, source),
    CHECK (
        (snapshot_schema_version = 1
            AND tombstone_safe_after_ingest_fence IS NULL
            AND deleted_session_count = 0)
        OR
        (snapshot_schema_version = 2
            AND tombstone_safe_after_ingest_fence IS NOT NULL
            AND tombstone_safe_after_ingest_fence <= ingest_fence)
    )
);

CREATE TABLE session_archive_applied_tombstones (
    tenant_id TEXT NOT NULL,
    source TEXT NOT NULL,
    source_session_id TEXT NOT NULL,
    deleted_at_ms BIGINT NOT NULL CHECK (deleted_at_ms >= 0),
    ingest_fence BIGINT NOT NULL CHECK (ingest_fence >= 0),
    deleted_records BIGINT NOT NULL CHECK (deleted_records >= 0),
    applied_at BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, source, source_session_id)
);

CREATE TABLE session_archive_source_sessions (
    tenant_id TEXT NOT NULL,
    source TEXT NOT NULL,
    source_session_id TEXT NOT NULL,
    requests BIGINT NOT NULL CHECK (requests > 0),
    first_at_ms BIGINT NOT NULL CHECK (first_at_ms >= 0),
    last_at_ms BIGINT NOT NULL CHECK (last_at_ms >= first_at_ms),
    records_sha256 TEXT NOT NULL,
    ingest_fence BIGINT NOT NULL CHECK (ingest_fence >= 0),
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, source, source_session_id)
);
