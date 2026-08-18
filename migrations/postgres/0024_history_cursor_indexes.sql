-- These parent partitioned indexes are part of the default PostgreSQL schema,
-- not an optional operator repair. PostgreSQL builds matching indexes for all
-- existing leaves transactionally and propagates them to partitions created
-- later by the normal maintenance path.
CREATE INDEX IF NOT EXISTS request_records_recent_idx
    ON request_records (created_at DESC, id DESC);

CREATE INDEX IF NOT EXISTS request_events_global_cursor_idx
    ON request_events (event_at ASC, event_id ASC);

COMMENT ON INDEX request_records_recent_idx IS
    'Default global newest-request access path for operator history queries.';
COMMENT ON INDEX request_events_global_cursor_idx IS
    'Default global request-event cursor access path for resumable monitoring.';
