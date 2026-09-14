ALTER TABLE key_records ADD COLUMN archived_at BIGINT;
ALTER TABLE key_records ADD COLUMN creation_source TEXT NOT NULL DEFAULT 'unknown'
    CHECK (creation_source IN ('manual', 'api', 'unknown'));
CREATE INDEX key_records_tenant_directory_idx
    ON key_records (tenant_id, archived_at, creation_source, status, created_at DESC, id DESC);
