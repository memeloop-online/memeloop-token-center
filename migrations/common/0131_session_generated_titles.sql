-- One bounded title and job state per credential-scoped session. No prompt,
-- copied history, tool body, or model response envelope is retained here.
CREATE TABLE session_generated_titles (
    tenant_id TEXT NOT NULL,
    key_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    title TEXT,
    state TEXT NOT NULL DEFAULT 'pending',
    revision BIGINT NOT NULL DEFAULT 1,
    attempts BIGINT NOT NULL DEFAULT 0,
    next_attempt_at BIGINT NOT NULL,
    lease_owner TEXT,
    lease_until BIGINT,
    error_code TEXT,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, key_id, session_id),
    CHECK (state IN ('pending', 'running', 'ready', 'failed', 'waiting_config')),
    CHECK (title IS NULL OR LENGTH(title) BETWEEN 1 AND 80)
);
CREATE INDEX session_generated_titles_due_idx
    ON session_generated_titles (next_attempt_at, tenant_id, key_id, session_id)
    WHERE state IN ('pending', 'running', 'waiting_config');
