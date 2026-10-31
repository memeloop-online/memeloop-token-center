ALTER TABLE session_generated_titles ADD COLUMN title_source TEXT NOT NULL DEFAULT 'generated';
ALTER TABLE session_generated_titles ADD COLUMN name_sync_id TEXT;
CREATE TABLE session_thread_parents (
    tenant_id TEXT NOT NULL,
    child_thread_id TEXT NOT NULL,
    parent_thread_id TEXT NOT NULL,
    evidence TEXT NOT NULL,
    PRIMARY KEY (tenant_id, child_thread_id),
    CHECK (child_thread_id <> parent_thread_id)
);
CREATE INDEX session_thread_parents_parent_idx
    ON session_thread_parents (tenant_id, parent_thread_id);
CREATE TABLE session_relationships (
    tenant_id TEXT NOT NULL,
    key_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    target_key_id TEXT NOT NULL,
    target_session_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('parent', 'related')),
    evidence TEXT,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, key_id, session_id, target_key_id, target_session_id, kind),
    CHECK (key_id <> target_key_id OR session_id <> target_session_id)
);
CREATE INDEX session_relationships_parent_idx
    ON session_relationships (tenant_id, key_id, session_id) WHERE kind = 'parent';
CREATE INDEX session_relationships_target_idx
    ON session_relationships (tenant_id, target_key_id, target_session_id, kind);
CREATE INDEX conversation_observations_explicit_session_idx
    ON conversation_observations (explicit_session_id, key_id, cluster_id);
CREATE INDEX request_stats_facts_session_idx
    ON request_stats_facts (tenant_id, key_id, session_id, created_at, request_id);
