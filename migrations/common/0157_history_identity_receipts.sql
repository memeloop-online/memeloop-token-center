-- Compact ownership and delivery proof; no request body or canonical payload.
CREATE TABLE request_history_identities (
    source TEXT NOT NULL,
    request_id TEXT NOT NULL,
    tenant_id TEXT NOT NULL,
    key_id TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    head_revision BIGINT NOT NULL,
    acknowledged_revision BIGINT,
    acknowledged_hash TEXT,
    PRIMARY KEY (source, request_id),
    CHECK (acknowledged_revision IS NULL OR acknowledged_revision <= head_revision)
);
CREATE INDEX request_history_identities_scope ON request_history_identities(tenant_id,key_id,source,request_id);
