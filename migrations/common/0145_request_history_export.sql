CREATE TABLE request_history_state (
    id BIGINT PRIMARY KEY CHECK (id = 1),
    mode TEXT NOT NULL DEFAULT 'off' CHECK (mode IN ('off', 'shadow', 'clickhouse')),
    verified_revision BIGINT,
    verification_json TEXT
);
INSERT INTO request_history_state (id) VALUES (1);

CREATE TABLE request_history_backfill (
    source TEXT PRIMARY KEY,
    after_id TEXT NOT NULL DEFAULT '',
    done BIGINT NOT NULL DEFAULT 0
);
INSERT INTO request_history_backfill (source) VALUES ('native'), ('generation'), ('archive');

CREATE VIEW request_history_source_ids AS
    SELECT 'native' AS source, id AS request_id, tenant_id, key_id, created_at FROM request_records
    UNION ALL
    SELECT 'generation', id, tenant_id, key_id, created_at FROM generation_jobs
    UNION ALL
    SELECT 'archive', archive_request_id, tenant_id, key_id, source_started_at FROM session_archive_unlinked_requests;
