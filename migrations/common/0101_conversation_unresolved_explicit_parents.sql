CREATE TABLE conversation_unresolved_explicit_parents (
    child_observation_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    principal_id TEXT NOT NULL,
    key_id TEXT NOT NULL,
    parent_reference TEXT NOT NULL,
    subagent BIGINT NOT NULL DEFAULT 0,
    created_at BIGINT NOT NULL
);

CREATE INDEX conversation_unresolved_explicit_parent_lookup_idx
    ON conversation_unresolved_explicit_parents (
        tenant_id,
        principal_id,
        key_id,
        parent_reference,
        created_at,
        child_observation_id
    );
