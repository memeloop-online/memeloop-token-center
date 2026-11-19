-- Request-scoped reading jobs reference shared semantic text atoms. No copied
-- response envelope, tool payload, or archive body is retained here.
CREATE TABLE session_response_summaries (
    tenant_id TEXT NOT NULL,
    key_id TEXT NOT NULL,
    request_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending',
    atom_hash TEXT,
    error_code TEXT,
    attempts BIGINT NOT NULL DEFAULT 0,
    next_attempt_at BIGINT NOT NULL,
    lease_owner TEXT,
    lease_until BIGINT,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, key_id, request_id),
    CHECK (state IN ('pending', 'running', 'ready', 'unavailable'))
);
CREATE INDEX session_response_summaries_due_idx
    ON session_response_summaries (next_attempt_at, tenant_id, key_id, request_id)
    WHERE state IN ('pending', 'running');
