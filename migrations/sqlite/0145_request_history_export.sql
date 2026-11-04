CREATE TABLE request_history_export (
    revision INTEGER PRIMARY KEY AUTOINCREMENT,
    source TEXT NOT NULL,
    request_id TEXT NOT NULL,
    tenant_id TEXT NOT NULL,
    key_id TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    snapshot TEXT,
    UNIQUE (source, request_id)
);
