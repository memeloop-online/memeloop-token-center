-- SQLite cannot widen a CHECK in place. Preserve all original identities,
-- payload bytes, length/digest metadata and the parent cascade relationship.
CREATE TABLE conversation_semantic_payloads_v153 (
    request_id TEXT PRIMARY KEY REFERENCES conversation_projection_outbox(request_id) ON DELETE CASCADE,
    tenant_id TEXT NOT NULL,
    key_id TEXT NOT NULL,
    principal_id TEXT NOT NULL,
    format_version BIGINT NOT NULL CHECK (format_version IN (1, 2)),
    encoded_bytes BIGINT NOT NULL CHECK (encoded_bytes >= 2 AND encoded_bytes <= 134217728),
    digest TEXT NOT NULL,
    request_json TEXT NOT NULL
);
INSERT INTO conversation_semantic_payloads_v153
    SELECT request_id, tenant_id, key_id, principal_id, format_version, encoded_bytes, digest, request_json
    FROM conversation_semantic_payloads;
DROP TABLE conversation_semantic_payloads;
ALTER TABLE conversation_semantic_payloads_v153 RENAME TO conversation_semantic_payloads;
