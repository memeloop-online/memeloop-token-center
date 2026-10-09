CREATE TABLE conversation_semantic_payloads (
    request_id TEXT PRIMARY KEY REFERENCES conversation_projection_outbox(request_id) ON DELETE CASCADE,
    tenant_id TEXT NOT NULL,
    key_id TEXT NOT NULL,
    principal_id TEXT NOT NULL,
    format_version BIGINT NOT NULL CHECK (format_version = 1),
    encoded_bytes BIGINT NOT NULL CHECK (encoded_bytes >= 2 AND encoded_bytes <= 134217728),
    digest TEXT NOT NULL,
    request_json TEXT NOT NULL
);
