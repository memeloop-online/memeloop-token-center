-- Bounded target-side staging for one sealed stable-snapshot transaction.
-- Importers delete these rows before commit and rollback also removes them.
ALTER TABLE session_archive_import_records
    ADD COLUMN previous_request_object TEXT;
ALTER TABLE session_archive_import_records
    ADD COLUMN previous_response_object TEXT;
ALTER TABLE session_archive_import_records
    ADD COLUMN previous_conversation_cluster_id TEXT;
ALTER TABLE session_archive_import_records
    ADD COLUMN conversation_observation_created BIGINT NOT NULL DEFAULT 0
        CHECK (conversation_observation_created IN (0, 1));

CREATE TABLE session_archive_snapshot_stage_sessions (
    batch_id TEXT NOT NULL,
    tenant_id TEXT NOT NULL,
    source TEXT NOT NULL,
    source_session_id TEXT NOT NULL,
    deleted BIGINT NOT NULL CHECK (deleted IN (0, 1)),
    requests BIGINT NOT NULL CHECK (requests >= 0),
    first_at_ms BIGINT,
    last_at_ms BIGINT NOT NULL,
    records_sha256 TEXT,
    deleted_at_ms BIGINT,
    deleted_records BIGINT NOT NULL DEFAULT 0 CHECK (deleted_records >= 0),
    PRIMARY KEY (batch_id, tenant_id, source, source_session_id)
);

CREATE TABLE session_archive_snapshot_stage_records (
    batch_id TEXT NOT NULL,
    tenant_id TEXT NOT NULL,
    source TEXT NOT NULL,
    source_session_id TEXT NOT NULL,
    external_request_id TEXT NOT NULL,
    record_digest TEXT NOT NULL,
    disposition TEXT NOT NULL CHECK (disposition IN ('exact', 'unlinked', 'quarantine')),
    PRIMARY KEY (batch_id, tenant_id, source, external_request_id)
);

CREATE INDEX session_archive_snapshot_stage_record_session_idx
    ON session_archive_snapshot_stage_records
       (batch_id, tenant_id, source, source_session_id, external_request_id);
